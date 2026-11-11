//! Bounded, session-only Microsoft 365 OneDrive file previews.
//!
//! The OneDrive directory adapter owns metadata only. This module is the
//! separate selected-read seam for one current root-level file: it follows the
//! Graph `/content` redirect exactly once, keeps authorization on Graph only,
//! validates and bounds the returned data, then exposes a presentation-safe
//! result with no route, redirect URL, provider body, or local path.

use super::GraphSession;
use super::mutation::is_dot_segment;
use super::{GraphContentRedirect, GraphContentResponse, GraphHttpError, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderResourceRef, RemoteFilePreviewPayload, RemoteFilePreviewProjection,
    RemoteFilePreviewUnavailable,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use crate::shared::file_preview::{
    MAX_IMAGE_PREVIEW_BYTES, MAX_IMAGE_PREVIEW_PIXELS, MAX_TEXT_PREVIEW_BYTES,
    next_preview_cache_key,
};
use std::io::Cursor;
use url::{Host, Url};
use zeroize::Zeroizing;

const MAX_PREAUTHORIZED_CONTENT_URL_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilePreviewReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    TooLarge,
    Unavailable,
}

impl Microsoft365Transport {
    /// Reads one explicitly selected file from the signed-in user's current
    /// OneDrive-root projection. This is deliberately not a directory refresh,
    /// mutation, browser handoff, or local Files operation.
    pub(super) fn poll_one_drive_file_preview(
        &mut self,
        item: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_one_drive_file_preview_with_cancellation(item, None)
    }

    /// Lets the serial transport worker discard a selected file result after a
    /// navigation or account transition. The signal is checked before Graph,
    /// between the Graph redirect and external content hop, and before a
    /// normalized result crosses the integration seam.
    pub(super) fn poll_one_drive_file_preview_cancellable(
        &mut self,
        item: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_one_drive_file_preview_with_cancellation(item, Some(cancellation))
    }

    fn poll_one_drive_file_preview_with_cancellation(
        &mut self,
        item: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let text_hint = item
            .microsoft365_id()
            .is_some_and(|id| self.one_drive_text_item_ids.contains(id));
        let (payload, notices) =
            match fetch_one_drive_file_preview(&self.session, &item, text_hint, cancellation) {
                Ok(payload) => (payload, Vec::new()),
                Err(FilePreviewReadFailure::Cancelled) => return Err(TransportError::Cancelled),
                Err(FilePreviewReadFailure::ReauthenticationRequired) => {
                    let error = TransportError::ReauthenticationRequired;
                    self.identity = None;
                    self.record_error(&error);
                    return Err(error);
                }
                Err(FilePreviewReadFailure::AccessDenied) => (
                    RemoteFilePreviewPayload::Unavailable(
                        RemoteFilePreviewUnavailable::AccessDenied,
                    ),
                    Vec::new(),
                ),
                Err(FilePreviewReadFailure::TooLarge) => (
                    RemoteFilePreviewPayload::Unavailable(RemoteFilePreviewUnavailable::TooLarge),
                    Vec::new(),
                ),
                Err(FilePreviewReadFailure::Throttled(_)) => (
                    RemoteFilePreviewPayload::Unavailable(
                        RemoteFilePreviewUnavailable::Unavailable,
                    ),
                    vec!["Microsoft Graph is temporarily throttling OneDrive file preview.".into()],
                ),
                Err(FilePreviewReadFailure::Unavailable) => (
                    RemoteFilePreviewPayload::Unavailable(
                        RemoteFilePreviewUnavailable::Unavailable,
                    ),
                    Vec::new(),
                ),
            };
        if check_cancelled(cancellation).is_err() {
            return Err(TransportError::Cancelled);
        }
        let projection = RemoteFilePreviewProjection::microsoft365(item.clone(), payload)?;
        Ok(WorkspaceDelta {
            completed: vec![SyncInterest::OneDriveFilePreview { item }],
            remote_file_preview: Some(projection),
            notices,
            ..WorkspaceDelta::default()
        })
    }
}

fn fetch_one_drive_file_preview(
    session: &GraphSession<'_>,
    item: &ProviderResourceRef,
    text_hint: bool,
    cancellation: Option<&TransportCancellation>,
) -> Result<RemoteFilePreviewPayload, FilePreviewReadFailure> {
    check_cancelled(cancellation)?;
    let graph_url = one_drive_file_content_url(session.cloud(), item)?;
    let redirect = session
        .get_content_redirect(&graph_url)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    let destination = validated_content_redirect(session.cloud(), redirect)?;
    check_cancelled(cancellation)?;
    // An item whose name is on the text allow-list never needs the larger
    // image cap, so bound the download itself rather than the decoded text.
    let download_cap = if text_hint {
        MAX_TEXT_PREVIEW_BYTES
    } else {
        MAX_IMAGE_PREVIEW_BYTES
    };
    let mut response = session
        .get_redirected_content(&destination, download_cap)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    match response.status {
        200 => normalize_preview_response(&mut response, text_hint),
        // This hop is deliberately unauthenticated: a 401 here says nothing
        // about the Graph bearer token that was accepted or rejected by the
        // preceding `/content` request. Only `validated_content_redirect`
        // may convert a Graph-hop 401 into a reauthentication transition.
        401 => Err(FilePreviewReadFailure::Unavailable),
        403 => Err(FilePreviewReadFailure::AccessDenied),
        _ => Err(FilePreviewReadFailure::Unavailable),
    }
}

fn one_drive_file_content_url(
    cloud: GraphCloud,
    item: &ProviderResourceRef,
) -> Result<Url, FilePreviewReadFailure> {
    let id = item
        .microsoft365_id()
        .ok_or(FilePreviewReadFailure::Unavailable)?;
    // `url` silently drops "." and ".." path segments, which would retarget
    // the request. Provider IDs are never dot segments.
    if is_dot_segment(id) {
        return Err(FilePreviewReadFailure::Unavailable);
    }
    cloud
        .graph_route(&["me", "drive", "items", id, "content"], None)
        .map_err(|_| FilePreviewReadFailure::Unavailable)
}

fn validated_content_redirect(
    cloud: GraphCloud,
    redirect: GraphContentRedirect,
) -> Result<Url, FilePreviewReadFailure> {
    match redirect.status {
        302 => {}
        401 => return Err(FilePreviewReadFailure::ReauthenticationRequired),
        403 => return Err(FilePreviewReadFailure::AccessDenied),
        429 => {
            return Err(FilePreviewReadFailure::Throttled(
                redirect.retry_after_seconds,
            ));
        }
        _ => return Err(FilePreviewReadFailure::Unavailable),
    }
    let Some(location) = redirect.location.as_deref() else {
        return Err(FilePreviewReadFailure::Unavailable);
    };
    preauthorized_content_url(cloud, location).ok_or(FilePreviewReadFailure::Unavailable)
}

/// The unauthenticated content hop goes only to a subdomain of one of the
/// configured cloud's download suffixes (`GraphCloud::content_host_suffixes`),
/// so a compromised or misconfigured redirect cannot steer the request (and
/// its response size and timing) elsewhere. Each national cloud allows only
/// its own SharePoint suffix, and the commercial-only `svc.ms` stays out of
/// them.
///
/// Matches on a DNS label boundary: `contoso-my.sharepoint.com` is allowed,
/// while `evilsharepoint.com`, `sharepoint.com.evil.test` and the bare apex
/// `sharepoint.com` are not. The host is already lowercase and has no
/// trailing root label.
fn is_microsoft_content_host(host: &str, cloud: GraphCloud) -> bool {
    cloud.content_host_suffixes().iter().any(|suffix| {
        host.strip_suffix(suffix)
            .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.'))
    })
}

/// Rejects a Graph-provided destination unless it has an explicit HTTPS
/// authority, no credential component, no fragment, and a DNS host that is a
/// subdomain of an allow-listed Microsoft download suffix. The
/// provider-issued URL is never returned outside this module or rendered.
pub(super) fn preauthorized_content_url(cloud: GraphCloud, value: &str) -> Option<Url> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_PREAUTHORIZED_CONTENT_URL_BYTES
        || trimmed.chars().any(char::is_control)
        || !has_explicit_https_authority(trimmed)
    {
        return None;
    }
    let url = Url::parse(trimmed).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    match url.host()? {
        Host::Domain(domain) => {
            // A DNS trailing root label is semantically equivalent to the
            // same hostname without it (for example, `localhost.`). Remove
            // it before the localhost policy check so the preauthenticated
            // request cannot be steered to a loopback alias.
            let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
            (!normalized.is_empty()
                && normalized != "localhost"
                && !normalized.ends_with(".localhost")
                && is_microsoft_content_host(&normalized, cloud))
            .then_some(url)
        }
        Host::Ipv4(_) | Host::Ipv6(_) => None,
    }
}

fn has_explicit_https_authority(value: &str) -> bool {
    let Some(scheme_end) = value.find(':') else {
        return false;
    };
    let Some(authority) = value.get(scheme_end + 1..) else {
        return false;
    };
    value[..scheme_end].eq_ignore_ascii_case("https")
        && authority.starts_with("//")
        && authority
            .get(2..)
            .is_some_and(|rest| !rest.starts_with('/'))
}

fn normalize_preview_response(
    response: &mut GraphContentResponse,
    text_hint: bool,
) -> Result<RemoteFilePreviewPayload, FilePreviewReadFailure> {
    let content_kind = match response.content_type.as_deref() {
        // OneDrive commonly labels plain text as a generic binary stream.
        // Trust it only for an allow-listed extension and only when the bytes
        // are UTF-8 without NULs; otherwise it stays unsupported.
        None | Some("") if text_hint => Some(PreviewContentKind::Text),
        Some(value) if text_hint && is_octet_stream(value) => Some(PreviewContentKind::Text),
        other => other.and_then(preview_content_kind),
    };
    let generic_text = text_hint
        && response
            .content_type
            .as_deref()
            .is_none_or(|value| value.is_empty() || is_octet_stream(value));
    let Some(content_kind) = content_kind else {
        return Ok(RemoteFilePreviewPayload::Unavailable(
            RemoteFilePreviewUnavailable::UnsupportedContent,
        ));
    };
    let body = response.take_body();
    if generic_text && body.contains(&0) {
        return Ok(RemoteFilePreviewPayload::Unavailable(
            RemoteFilePreviewUnavailable::UnsupportedContent,
        ));
    }
    match content_kind {
        PreviewContentKind::Text => text_preview_payload(&body),
        PreviewContentKind::Image => image_preview_payload(&body),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewContentKind {
    Text,
    Image,
}

fn is_octet_stream(value: &str) -> bool {
    value.split(';').next().is_some_and(|media| {
        media
            .trim()
            .eq_ignore_ascii_case("application/octet-stream")
    })
}

fn preview_content_kind(value: &str) -> Option<PreviewContentKind> {
    let media_type = value.split(';').next()?.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" => {
            Some(PreviewContentKind::Image)
        }
        "application/json" | "application/xml" | "application/javascript" | "text/xml" => {
            Some(PreviewContentKind::Text)
        }
        "text/html" => None,
        _ if media_type.starts_with("text/") => Some(PreviewContentKind::Text),
        _ => None,
    }
}

fn text_preview_payload(
    body: &Zeroizing<Vec<u8>>,
) -> Result<RemoteFilePreviewPayload, FilePreviewReadFailure> {
    if body.len() as u64 > MAX_TEXT_PREVIEW_BYTES {
        return Err(FilePreviewReadFailure::TooLarge);
    }
    match std::str::from_utf8(body) {
        Ok(value) => Ok(RemoteFilePreviewPayload::Text(sanitize_preview_text(value))),
        Err(_) => Ok(RemoteFilePreviewPayload::Unavailable(
            RemoteFilePreviewUnavailable::InvalidContent,
        )),
    }
}

fn sanitize_preview_text(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    let mut previous_was_carriage_return = false;
    for character in value.chars() {
        match character {
            '\r' => {
                text.push('\n');
                previous_was_carriage_return = true;
            }
            '\n' if previous_was_carriage_return => previous_was_carriage_return = false,
            '\n' | '\t' => {
                text.push(character);
                previous_was_carriage_return = false;
            }
            character
                if character.is_control() || super::text::is_deceptive_format_char(character) =>
            {
                previous_was_carriage_return = false;
            }
            character => {
                text.push(character);
                previous_was_carriage_return = false;
            }
        }
    }
    text
}

fn image_preview_payload(
    body: &Zeroizing<Vec<u8>>,
) -> Result<RemoteFilePreviewPayload, FilePreviewReadFailure> {
    if body.len() as u64 > MAX_IMAGE_PREVIEW_BYTES {
        return Err(FilePreviewReadFailure::TooLarge);
    }
    let reader = match image::ImageReader::new(Cursor::new(body.as_slice())).with_guessed_format() {
        Ok(reader) => reader,
        Err(_) => {
            return Ok(RemoteFilePreviewPayload::Unavailable(
                RemoteFilePreviewUnavailable::InvalidContent,
            ));
        }
    };
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(_) => {
            return Ok(RemoteFilePreviewPayload::Unavailable(
                RemoteFilePreviewUnavailable::InvalidContent,
            ));
        }
    };
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > MAX_IMAGE_PREVIEW_PIXELS {
        return Err(FilePreviewReadFailure::TooLarge);
    }
    Ok(RemoteFilePreviewPayload::Image {
        bytes: body.as_slice().into(),
        cache_key: next_preview_cache_key(body),
    })
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), FilePreviewReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(FilePreviewReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> FilePreviewReadFailure {
    match error {
        GraphHttpError::ResponseTooLarge => FilePreviewReadFailure::TooLarge,
        GraphHttpError::Network
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => FilePreviewReadFailure::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::super::GraphHttpResponse;
    use super::{
        FilePreviewReadFailure, GraphContentRedirect, GraphContentResponse, GraphHttpError,
        Microsoft365Transport, RemoteFilePreviewPayload, TransportCancellation,
        fetch_one_drive_file_preview, one_drive_file_content_url, preauthorized_content_url,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, RemoteDriveItem, RemoteDriveItemKind, SyncInterest,
        TransportError, WorkspaceTransport,
    };
    use std::sync::Mutex;
    use url::Url;
    use zeroize::Zeroizing;

    struct TestHttpClient {
        requested_graph_urls: Mutex<Vec<String>>,
        requested_content_urls: Mutex<Vec<String>>,
        requested_caps: Mutex<Vec<u64>>,
        redirect: GraphContentRedirect,
        content: GraphContentResponse,
    }

    impl GraphHttpClient for TestHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_content_redirect(
            &self,
            url: &Url,
            access_token: &str,
        ) -> Result<GraphContentRedirect, GraphHttpError> {
            assert_eq!(access_token, "test-token");
            self.requested_graph_urls
                .lock()
                .expect("graph URL list must not be poisoned")
                .push(url.as_str().into());
            Ok(GraphContentRedirect {
                status: self.redirect.status,
                retry_after_seconds: self.redirect.retry_after_seconds,
                location: self
                    .redirect
                    .location
                    .as_ref()
                    .map(|value| Zeroizing::new(value.to_string())),
            })
        }

        fn get_redirected_content(
            &self,
            url: &Url,
            max_response_bytes: u64,
        ) -> Result<GraphContentResponse, GraphHttpError> {
            self.requested_caps
                .lock()
                .expect("cap list must not be poisoned")
                .push(max_response_bytes);
            self.requested_content_urls
                .lock()
                .expect("content URL list must not be poisoned")
                .push(url.as_str().into());
            Ok(GraphContentResponse {
                status: self.content.status,
                content_type: self.content.content_type.clone(),
                body: self.content.body.clone(),
            })
        }
    }

    fn remote_item() -> crate::integrations::ProviderResourceRef {
        RemoteDriveItem::for_test(
            "item / needs encoding",
            "Notes.txt",
            "2026-11-06T10:30:00Z",
            Some(12),
            RemoteDriveItemKind::File,
        )
        .target()
    }

    fn client(location: &str, content_type: &str, body: &[u8]) -> TestHttpClient {
        TestHttpClient {
            requested_graph_urls: Mutex::new(Vec::new()),
            requested_content_urls: Mutex::new(Vec::new()),
            requested_caps: Mutex::new(Vec::new()),
            redirect: GraphContentRedirect {
                status: 302,
                retry_after_seconds: None,
                location: Some(Zeroizing::new(location.into())),
            },
            content: GraphContentResponse {
                status: 200,
                content_type: Some(content_type.into()),
                body: body.into(),
            },
        }
    }

    #[test]
    fn one_drive_content_route_uses_one_encoded_item_segment() {
        let url = one_drive_file_content_url(GraphCloud::Global, &remote_item())
            .expect("a normalized OneDrive item must have a content route");

        assert_eq!(
            url.as_str(),
            "https://graph.microsoft.com/v1.0/me/drive/items/item%20%2F%20needs%20encoding/content"
        );
    }

    #[test]
    fn selected_text_preview_uses_the_graph_hop_then_one_unauthenticated_destination() {
        let client = client(
            "https://contoso.sharepoint.com/download/preauthorized-token",
            "text/plain; charset=utf-8",
            b"A remote note\r\nwithout unsafe controls\x01.",
        );
        let result = fetch_one_drive_file_preview(
            &GraphSession::for_test(&client),
            &remote_item(),
            false,
            None,
        )
        .expect("the bounded text response must normalize");

        assert_eq!(
            result,
            RemoteFilePreviewPayload::Text("A remote note\nwithout unsafe controls.".into())
        );
        assert_eq!(
            client
                .requested_graph_urls
                .lock()
                .expect("graph URL list must not be poisoned")
                .len(),
            1
        );
        assert_eq!(
            client
                .requested_content_urls
                .lock()
                .expect("content URL list must not be poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn unsafe_or_relative_redirects_never_reach_the_content_hop() {
        for value in [
            "http://contoso.sharepoint.com/download",
            "https://localhost/download",
            "https://localhost./download",
            "https://tenant.localhost./download",
            "https://127.0.0.1/download",
            "https://name:password@contoso.sharepoint.com/download",
            "/relative/download",
            "https:/missing-authority",
            "https://contoso.sharepoint.com/download#fragment",
        ] {
            assert!(
                preauthorized_content_url(GraphCloud::Global, value).is_none(),
                "{value}"
            );
        }
    }

    #[test]
    fn only_microsoft_download_hosts_receive_the_content_hop() {
        for value in [
            "https://contoso.sharepoint.com/download",
            "https://contoso-my.sharepoint.com/personal/ada/_layouts/15/download.aspx?x=1",
            "https://CONTOSO.SharePoint.COM./download",
            "https://public.dm.files.1drv.com/y4m/file",
            "https://onedrive.1drv.com/download",
            "https://abc.livefilestore.com/y/file",
            "https://tenant.svc.ms/download",
            "https://my.microsoftpersonalcontent.com/personal/x/download",
        ] {
            assert!(
                preauthorized_content_url(GraphCloud::Global, value).is_some(),
                "{value}"
            );
        }
        for value in [
            "https://evil.test/download",
            "https://evilsharepoint.com/download",
            "https://sharepoint.com.evil.test/download",
            "https://contoso.sharepoint.com.evil.test/download",
            "https://sharepoint.com/download",
            "https://1drv.com/download",
            "https://contoso.sharepoint.com@evil.test/download",
            "https://sharepoint.cn/download",
            "https://contoso.sharepoint.us/download",
            "https://contoso.sharepoint-df.com/download",
            "https://xn--sharepoint-9ya.com/download",
            "https://graph.microsoft.com/download",
            "https://example.com/contoso.sharepoint.com/download",
        ] {
            assert!(
                preauthorized_content_url(GraphCloud::Global, value).is_none(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_redirect_to_a_foreign_host_is_unavailable_and_issues_no_content_request() {
        let client = client(
            "https://evil.test/steal",
            "text/plain",
            b"must never be fetched",
        );
        assert_eq!(
            fetch_one_drive_file_preview(
                &GraphSession::for_test(&client),
                &remote_item(),
                false,
                None
            ),
            Err(FilePreviewReadFailure::Unavailable)
        );
        assert!(
            client
                .requested_content_urls
                .lock()
                .expect("content URL list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn oversized_text_is_rejected_after_the_global_bounded_hop() {
        let bytes = vec![b'x'; super::MAX_TEXT_PREVIEW_BYTES as usize + 1];
        let client = client(
            "https://contoso.sharepoint.com/download/preauthorized-token",
            "text/plain",
            &bytes,
        );

        assert_eq!(
            fetch_one_drive_file_preview(
                &GraphSession::for_test(&client),
                &remote_item(),
                false,
                None
            ),
            Err(FilePreviewReadFailure::TooLarge)
        );
    }

    #[test]
    fn unauthenticated_content_hop_does_not_invalidate_the_graph_identity() {
        let client = TestHttpClient {
            requested_graph_urls: Mutex::new(Vec::new()),
            requested_content_urls: Mutex::new(Vec::new()),
            requested_caps: Mutex::new(Vec::new()),
            redirect: GraphContentRedirect {
                status: 302,
                retry_after_seconds: None,
                location: Some(Zeroizing::new(
                    "https://contoso.sharepoint.com/download/preauthorized-token".into(),
                )),
            },
            content: GraphContentResponse {
                status: 401,
                content_type: None,
                body: Vec::new(),
            },
        };
        let identity = AccountIdentity {
            account_id: "account-id".into(),
            display_name: "Ada Lovelace".into(),
            email: Some("ada@example.test".into()),
            tenant_id: None,
        };
        let mut transport = Microsoft365Transport::with_client("test-token", Box::new(client));
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity.clone());

        let delta = transport
            .poll(SyncInterest::OneDriveFilePreview {
                item: remote_item(),
            })
            .expect("a preauthorized-host 401 must become an unavailable preview");

        assert!(delta.remote_file_preview.is_some());
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(current) if current == identity
        ));
        assert_eq!(transport.identity, Some(identity));
    }

    #[test]
    fn graph_content_hop_401_requires_reauthentication_and_clears_identity() {
        let client = TestHttpClient {
            requested_graph_urls: Mutex::new(Vec::new()),
            requested_content_urls: Mutex::new(Vec::new()),
            requested_caps: Mutex::new(Vec::new()),
            redirect: GraphContentRedirect {
                status: 401,
                retry_after_seconds: None,
                location: None,
            },
            content: GraphContentResponse {
                status: 200,
                content_type: Some("text/plain".into()),
                body: b"must not reach the preauthorized host".to_vec(),
            },
        };
        let identity = AccountIdentity {
            account_id: "account-id".into(),
            display_name: "Ada Lovelace".into(),
            email: Some("ada@example.test".into()),
            tenant_id: None,
        };
        let mut transport = Microsoft365Transport::with_client("test-token", Box::new(client));
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);

        assert_eq!(
            transport.poll(SyncInterest::OneDriveFilePreview {
                item: remote_item(),
            }),
            Err(TransportError::ReauthenticationRequired)
        );
        assert!(transport.identity.is_none());
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::ReauthenticationRequired
        ));
    }

    #[test]
    fn cancelled_preview_stops_before_either_network_hop() {
        let client = client(
            "https://contoso.sharepoint.com/download/preauthorized-token",
            "text/plain",
            b"not read",
        );
        let cancellation = TransportCancellation::new();
        cancellation.cancel();

        assert_eq!(
            fetch_one_drive_file_preview(
                &GraphSession::for_test(&client),
                &remote_item(),
                false,
                Some(&cancellation),
            ),
            Err(FilePreviewReadFailure::Cancelled)
        );
        assert!(
            client
                .requested_graph_urls
                .lock()
                .expect("graph URL list must not be poisoned")
                .is_empty()
        );
    }

    fn dot_item(id: &str) -> crate::integrations::ProviderResourceRef {
        RemoteDriveItem::for_test(
            id,
            "Notes.txt",
            "2026-11-06T10:30:00Z",
            Some(12),
            RemoteDriveItemKind::File,
        )
        .target()
    }

    #[test]
    fn dot_segment_item_ids_are_rejected_before_building_a_route() {
        for id in [".", "..", " .. "] {
            assert_eq!(
                one_drive_file_content_url(GraphCloud::Global, &dot_item(id)),
                Err(FilePreviewReadFailure::Unavailable),
                "{id:?}"
            );
        }
    }

    fn octet_client(content_type: Option<&str>, body: &[u8]) -> TestHttpClient {
        let mut client = client(
            "https://contoso.sharepoint.com/download/token",
            "application/octet-stream",
            body,
        );
        client.content.content_type = content_type.map(str::to_owned);
        client
    }

    #[test]
    fn octet_stream_text_extension_valid_utf8_previews_as_text_with_text_cap() {
        for content_type in [
            Some("application/octet-stream"),
            Some("Application/Octet-Stream; x=y"),
            None,
        ] {
            let client = octet_client(content_type, b"hello \xE2\x80\xAEevil\r\nworld");
            let result = fetch_one_drive_file_preview(
                &GraphSession::for_test(&client),
                &remote_item(),
                true,
                None,
            )
            .expect("hinted text must normalize");
            assert_eq!(
                result,
                RemoteFilePreviewPayload::Text("hello evil\nworld".into())
            );
            assert_eq!(
                *client.requested_caps.lock().expect("caps"),
                vec![super::MAX_TEXT_PREVIEW_BYTES]
            );
        }
    }

    #[test]
    fn octet_stream_with_nul_or_without_text_hint_is_unsupported() {
        let nul = octet_client(Some("application/octet-stream"), b"ab\0cd");
        assert_eq!(
            fetch_one_drive_file_preview(&GraphSession::for_test(&nul), &remote_item(), true, None),
            Ok(RemoteFilePreviewPayload::Unavailable(
                crate::integrations::transport::RemoteFilePreviewUnavailable::UnsupportedContent
            ))
        );
        let unhinted = octet_client(Some("application/octet-stream"), b"plain text");
        assert_eq!(
            fetch_one_drive_file_preview(
                &GraphSession::for_test(&unhinted),
                &remote_item(),
                false,
                None
            ),
            Ok(RemoteFilePreviewPayload::Unavailable(
                crate::integrations::transport::RemoteFilePreviewUnavailable::UnsupportedContent
            ))
        );
        assert_eq!(
            *unhinted.requested_caps.lock().expect("caps"),
            vec![super::MAX_IMAGE_PREVIEW_BYTES]
        );
    }

    #[test]
    fn preview_text_strips_bidi_and_zero_width_but_keeps_joiners() {
        let sanitized = super::sanitize_preview_text(
            "a\u{202E}b\u{2066}c\u{200B}d\u{FEFF}e\u{061C}f\u{200E}g\u{2060}h\u{200D}i\u{200C}j",
        );
        assert_eq!(sanitized, "abcdefgh\u{200D}i\u{200C}j");
    }

    #[test]
    fn content_route_and_download_allowlist_follow_the_cloud() {
        for cloud in GraphCloud::ALL {
            let url = one_drive_file_content_url(cloud, &remote_item())
                .unwrap_or_else(|_| panic!("content route must build"));
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
        }
        let national = [
            (GraphCloud::UsGov, "sharepoint.us"),
            (GraphCloud::UsGovDod, "sharepoint-mil.us"),
            (GraphCloud::China, "sharepoint.cn"),
        ];
        for (cloud, suffix) in national {
            let allowed = format!("https://contoso.{suffix}/download");
            assert!(
                preauthorized_content_url(cloud, &allowed).is_some(),
                "{allowed}"
            );
            // Commercial-only and cross-cloud hosts are rejected.
            for other in [
                "https://contoso.sharepoint.com/download",
                "https://tenant.svc.ms/download",
                "https://tenant.dps.mil/download",
                "https://public.dm.files.1drv.com/y4m/file",
            ] {
                assert!(
                    preauthorized_content_url(cloud, other).is_none(),
                    "{cloud:?} {other}"
                );
            }
            for (other_cloud, other_suffix) in national {
                if other_cloud != cloud {
                    let cross = format!("https://contoso.{other_suffix}/download");
                    assert!(
                        preauthorized_content_url(cloud, &cross).is_none(),
                        "{cross}"
                    );
                }
            }
            // The commercial cloud never accepts a national host.
            assert!(preauthorized_content_url(GraphCloud::Global, &allowed).is_none());
        }
    }
}
