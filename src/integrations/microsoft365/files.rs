//! Microsoft Graph OneDrive-root metadata normalization.
//!
//! This module reads one bounded page of the signed-in user's OneDrive root.
//! It deliberately returns metadata plus an optional, strictly normalized
//! browser-display URL. Selected content preview is intentionally implemented
//! by the separate `file_preview` adapter; this listing never carries content
//! URLs, bytes, local paths, downloads, uploads, sharing state, recursive
//! browsing, or mutations across the integration boundary.

use super::GraphSession;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderBrowserUrl, ProviderResourceRef, RemoteDriveItem, RemoteDriveItemKind,
    RemoteFilesProjection,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

const MAX_FILES_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_REMOTE_DRIVE_ITEMS: usize = 50;
const MAX_ITEM_NAME_BYTES: usize = 512;
const MAX_TIMESTAMP_BYTES: usize = 96;

/// A bounded metadata projection plus non-sensitive notices about skipped or
/// truncated provider data. Raw Graph bodies, cursors, and routes stay here.
struct FilesReadResult {
    projection: RemoteFilesProjection,
    notices: Vec<String>,
}

/// File-read failures stay category-only so a Graph response body cannot leak
/// into a toast, feature state, or persisted projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilesReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl FilesReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph OneDrive metadata synchronization is temporarily unavailable"
                    .into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one bounded, session-only metadata page from the signed-in
    /// user's OneDrive root. It intentionally does not treat the listing as a
    /// "recent" view and never traverses folders or follows a continuation.
    pub(super) fn poll_files(&mut self) -> Result<WorkspaceDelta, TransportError> {
        self.poll_files_with_cancellation(None)
    }

    /// Lets the serial remote worker suppress a OneDrive projection when an
    /// account or profile transition occurs while the single request is in
    /// flight.
    pub(super) fn poll_files_cancellable(
        &mut self,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_files_with_cancellation(Some(cancellation))
    }

    fn poll_files_with_cancellation(
        &mut self,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }
        match fetch_files_projection(&self.session, cancellation)
        {
            Ok(result) => {
                self.one_drive_text_item_ids = result
                    .projection
                    .items()
                    .iter()
                    .filter(|item| {
                        item.kind() == RemoteDriveItemKind::File
                            && crate::shared::file_preview::has_text_preview_extension(
                                item.name(),
                            )
                    })
                    .filter_map(|item| item.target().microsoft365_id().map(str::to_owned))
                    .collect();
                Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Files],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: Some(result.projection),
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            })
            }
            Err(FilesReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Preserve a verified Graph identity while truthfully
                // replacing any old OneDrive metadata with an access state.
                completed: vec![SyncInterest::Files],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: Some(RemoteFilesProjection::access_denied()),
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to OneDrive metadata. Verify delegated Files.Read access and your account permissions, then reconnect."
                        .into(),
                ],
            }),
            Err(FilesReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(FilesReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn fetch_files_projection(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<FilesReadResult, FilesReadFailure> {
    check_cancelled(cancellation)?;
    let (items, truncated, mut skipped_items) = fetch_root_children(session, cancellation)?;
    let mut normalized = Vec::with_capacity(items.len());
    for item in items {
        check_cancelled(cancellation)?;
        if let Some(item) = normalize_drive_item(item) {
            normalized.push(item);
        } else {
            skipped_items = skipped_items.saturating_add(1);
        }
    }

    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Microsoft 365 OneDrive sync is showing at most {MAX_REMOTE_DRIVE_ITEMS} root items."
        ));
    }
    if skipped_items > 0 {
        notices.push(
            "Microsoft Graph returned some OneDrive root items without usable identifiers or names; they were skipped."
                .into(),
        );
    }
    check_cancelled(cancellation)?;
    Ok(FilesReadResult {
        projection: RemoteFilesProjection::microsoft365(normalized),
        notices,
    })
}

/// Performs exactly one children request. `@odata.nextLink` is intentionally
/// observed only to report truncation; it is never parsed or followed, so a
/// provider cursor cannot widen the bounded OneDrive-root contract.
fn fetch_root_children(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphDriveItem>, bool, usize), FilesReadFailure> {
    let url = one_drive_root_children_url(session.cloud())?;
    let response = get_json(session, &url, cancellation)?;
    let collection = decode_collection(&response.body)?;
    let mut items = Vec::with_capacity(collection.value.len().min(MAX_REMOTE_DRIVE_ITEMS));
    let mut seen = BTreeSet::new();
    let mut skipped_items = 0_usize;
    let truncated =
        collection.next_link.is_some() || collection.value.len() > MAX_REMOTE_DRIVE_ITEMS;

    for item in collection.value.into_iter().take(MAX_REMOTE_DRIVE_ITEMS) {
        let Some(id) = item.id.as_deref().and_then(normalized_identifier) else {
            skipped_items = skipped_items.saturating_add(1);
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        items.push(item);
    }
    check_cancelled(cancellation)?;
    Ok((items, truncated, skipped_items))
}

fn one_drive_root_children_url(cloud: GraphCloud) -> Result<Url, FilesReadFailure> {
    let mut url = cloud
        .graph_route(&["me", "drive", "root", "children"], None)
        .map_err(|_| FilesReadFailure::Unavailable)?;
    // This endpoint supports `$select` and `$top`. Keep the response strictly
    // metadata-only and bounded; no `$expand`, navigation, or content route
    // is part of the initial OneDrive contract.
    url.query_pairs_mut()
        .append_pair(
            "$select",
            "id,name,lastModifiedDateTime,size,file,folder,webUrl",
        )
        .append_pair("$top", &MAX_REMOTE_DRIVE_ITEMS.to_string());
    Ok(url)
}

fn normalize_drive_item(item: GraphDriveItem) -> Option<RemoteDriveItem> {
    let id = item.id.as_deref().and_then(normalized_identifier)?;
    let name = item
        .name
        .as_deref()
        .map(|value| safe_display(value, MAX_ITEM_NAME_BYTES))
        .filter(|value| !value.is_empty())?;
    let modified_at = item
        .last_modified_date_time
        .as_deref()
        .map(|value| safe_display(value, MAX_TIMESTAMP_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Modified time unavailable".into());
    let size_bytes = item.size.as_ref().and_then(Value::as_u64);
    let kind = classify_drive_item_kind(item.file.as_ref(), item.folder.as_ref());
    let web_url = item
        .web_url
        .as_deref()
        .and_then(ProviderBrowserUrl::microsoft365);
    RemoteDriveItem::microsoft365_with_web_url(
        ProviderResourceRef::microsoft365(id),
        name,
        modified_at,
        size_bytes,
        kind,
        web_url,
    )
    .ok()
}

fn classify_drive_item_kind(file: Option<&Value>, folder: Option<&Value>) -> RemoteDriveItemKind {
    match (
        file.is_some_and(Value::is_object),
        folder.is_some_and(Value::is_object),
    ) {
        (true, false) => RemoteDriveItemKind::File,
        (false, true) => RemoteDriveItemKind::Folder,
        _ => RemoteDriveItemKind::Unknown,
    }
}

fn get_json(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphHttpResponse, FilesReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_FILES_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_response_status(&response)?;
    Ok(response)
}

fn check_cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), FilesReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(FilesReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> FilesReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => FilesReadFailure::Unavailable,
    }
}

fn validate_response_status(response: &GraphHttpResponse) -> Result<(), FilesReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(FilesReadFailure::ReauthenticationRequired),
        403 => Err(FilesReadFailure::AccessDenied),
        429 => Err(FilesReadFailure::Throttled(response.retry_after_seconds)),
        _ => Err(FilesReadFailure::Unavailable),
    }
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection(body: &[u8]) -> Result<GraphCollection, FilesReadFailure> {
    serde_json::from_slice::<GraphCollection>(body).map_err(|_| FilesReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

#[derive(Deserialize)]
struct GraphCollection {
    value: Vec<GraphDriveItem>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDriveItem {
    id: Option<String>,
    name: Option<String>,
    last_modified_date_time: Option<String>,
    size: Option<Value>,
    file: Option<Value>,
    folder: Option<Value>,
    web_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        FilesReadFailure, GraphHttpError, GraphHttpResponse, Microsoft365Transport,
        RemoteDriveItemKind, SyncInterest, TransportCancellation, fetch_files_projection,
        one_drive_root_children_url,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, ProviderBrowserUrl, WorkspaceTransport,
    };
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use url::Url;

    struct TestHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requested_urls: Mutex<Vec<String>>,
    }

    impl TestHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
            }
        }
    }

    impl GraphHttpClient for TestHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .push(url.as_str().into());
            self.responses
                .lock()
                .expect("test response queue must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    struct CancellingHttpClient {
        cancellation: TransportCancellation,
        requests: AtomicUsize,
    }

    impl GraphHttpClient for CancellingHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            _url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.cancellation.cancel();
            response(200, r#"{"value":[]}"#)
        }
    }

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn root_metadata_uses_only_the_documented_bounded_route_and_safe_fields() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":"file-1","name":"  Release\nplan.docx  ","lastModifiedDateTime":"2026-11-06T10:30:00Z","size":42,"file":{},"webUrl":" https://contoso.sharepoint.com/:w:/r/personal/ada_contoso_com/Documents/Release%20plan.docx "},{"id":"folder-1","name":"Design","lastModifiedDateTime":"2026-11-06T09:00:00Z","size":0,"folder":{},"webUrl":"http://contoso.sharepoint.com/sites/Design"},{"id":"unknown-1","name":"Package","lastModifiedDateTime":"2026-11-06T08:00:00Z","size":-1,"file":{},"folder":{}}]}"#,
        )]);

        let result = fetch_files_projection(&GraphSession::for_test(&client), None)
            .expect("valid OneDrive metadata must normalize");

        assert_eq!(result.projection.items().len(), 3);
        let items = result.projection.items();
        assert_eq!(items[0].name(), "Release plan.docx");
        assert_eq!(items[0].modified_at(), "2026-11-06T10:30:00Z");
        assert_eq!(items[0].size_bytes(), Some(42));
        assert_eq!(items[0].kind(), RemoteDriveItemKind::File);
        assert_eq!(
            items[0]
                .web_url()
                .map(<ProviderBrowserUrl as AsRef<str>>::as_ref),
            Some(
                "https://contoso.sharepoint.com/:w:/r/personal/ada_contoso_com/Documents/Release%20plan.docx"
            )
        );
        assert_eq!(items[1].kind(), RemoteDriveItemKind::Folder);
        assert_eq!(items[1].web_url(), None);
        assert_eq!(items[2].size_bytes(), None);
        assert_eq!(items[2].kind(), RemoteDriveItemKind::Unknown);
        assert!(result.notices.is_empty());

        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested.len(), 1);
        let url = Url::parse(&requested[0]).expect("root metadata request must be a URL");
        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://graph.microsoft.com/v1.0/me/drive/root/children")
        );
        let query = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.len(), 2);
        assert_eq!(query[0].0, "$select");
        assert_eq!(
            query[0].1,
            "id,name,lastModifiedDateTime,size,file,folder,webUrl"
        );
        assert_eq!(query[1].0, "$top");
        assert_eq!(query[1].1, "50");
    }

    #[test]
    fn provider_web_urls_require_bounded_explicit_https_without_credentials() {
        assert_eq!(
            ProviderBrowserUrl::microsoft365(
                " https://tenant.sharepoint.com/sites/Team/Shared%20Documents/Plan.docx ",
            )
            .as_ref()
            .map(<ProviderBrowserUrl as AsRef<str>>::as_ref),
            Some("https://tenant.sharepoint.com/sites/Team/Shared%20Documents/Plan.docx")
        );
    }

    #[test]
    fn malformed_provider_web_urls_are_dropped_without_dropping_metadata() {
        let too_long = format!("https://tenant.sharepoint.com/{}", "x".repeat(8 * 1024));
        for (index, value) in [
            "http://tenant.sharepoint.com/sites/Team",
            "https:/tenant.sharepoint.com/sites/Team",
            "https:////tenant.sharepoint.com/sites/Team",
            "https://user:pass@tenant.sharepoint.com/sites/Team",
            "https://tenant.sharepoint.com/\nnext",
            "https://",
        ]
        .into_iter()
        .chain(std::iter::once(too_long.as_str()))
        .enumerate()
        {
            let item = super::normalize_drive_item(super::GraphDriveItem {
                id: Some(format!("item-{index}")),
                name: Some("Valid document.docx".into()),
                last_modified_date_time: Some("2026-11-06T10:30:00Z".into()),
                size: None,
                file: None,
                folder: None,
                web_url: Some(value.into()),
            })
            .expect("an invalid optional browser URL must not reject valid metadata");
            assert_eq!(item.web_url(), None);
        }
    }

    #[test]
    fn root_metadata_stops_at_one_page_and_reports_truncation() {
        let values = (0..51)
            .map(|index| {
                format!(
                    r#"{{"id":"item-{index}","name":"Item {index}","lastModifiedDateTime":"2026-11-06T10:30:00Z","file":{{}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let client = TestHttpClient::new([response(
            200,
            &format!(
                r#"{{"value":[{values}],"@odata.nextLink":"https://example.test/not-followed"}}"#
            ),
        )]);

        let result = fetch_files_projection(&GraphSession::for_test(&client), None)
            .expect("the first OneDrive page must normalize");

        assert_eq!(result.projection.items().len(), 50);
        assert!(result.notices[0].contains("at most 50 root items"));
        assert_eq!(
            client
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn malformed_root_items_are_skipped_without_leaking_raw_values() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":" ","name":"Missing ID","lastModifiedDateTime":"2026-11-06T10:30:00Z","file":{}},{"id":"valid-id","name":"\u0000","lastModifiedDateTime":"2026-11-06T10:30:00Z","file":{}},{"id":"valid-item","name":"Visible","lastModifiedDateTime":"\u0000","file":{}}]}"#,
        )]);

        let result = fetch_files_projection(&GraphSession::for_test(&client), None)
            .expect("a malformed item must not fail the bounded listing");

        assert_eq!(result.projection.items().len(), 1);
        assert_eq!(result.projection.items()[0].name(), "Visible");
        assert_eq!(
            result.projection.items()[0].modified_at(),
            "Modified time unavailable"
        );
        assert!(result.notices[0].contains("without usable identifiers or names"));
    }

    #[test]
    fn cancellation_discards_a_completed_root_metadata_response() {
        let cancellation = TransportCancellation::new();
        let client = CancellingHttpClient {
            cancellation: cancellation.clone(),
            requests: AtomicUsize::new(0),
        };

        let result = fetch_files_projection(&GraphSession::for_test(&client), Some(&cancellation));

        assert!(matches!(result, Err(FilesReadFailure::Cancelled)));
        assert_eq!(client.requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn access_denial_replaces_metadata_without_losing_verified_identity() {
        let client =
            Box::new(TestHttpClient::new([response(403, "{}")])) as Box<dyn GraphHttpClient>;
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity.clone());

        let delta = transport
            .poll(SyncInterest::Files)
            .expect("OneDrive access denial must retain a verified identity");

        let projection = delta
            .remote_files
            .expect("an access-denied remote files projection is expected");
        assert!(projection.is_access_denied());
        assert!(projection.items().is_empty());
        assert!(delta.notices[0].contains("Files.Read"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(current) if current.account_id == "u1"
        ));
    }

    #[test]
    fn built_in_root_url_is_valid() {
        assert!(one_drive_root_children_url(GraphCloud::Global,).is_ok());
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url = super::one_drive_root_children_url(cloud)
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}

#[cfg(test)]
mod decode_collection_tests {
    use super::{FilesReadFailure, decode_collection};

    #[test]
    fn decode_collection_reads_a_page_and_maps_malformed_bodies_to_unavailable() {
        let page =
            decode_collection(br#"{"value":[],"@odata.nextLink":"https://example.invalid/n"}"#)
                .expect("an empty page decodes");
        assert!(page.value.is_empty());
        assert_eq!(page.next_link.as_deref(), Some("https://example.invalid/n"));
        for body in [&b""[..], b"null", b"[]", b"{\"value\":3}", b"\xff\xfe"] {
            assert!(matches!(
                decode_collection(body),
                Err(FilesReadFailure::Unavailable)
            ));
        }
    }
}
