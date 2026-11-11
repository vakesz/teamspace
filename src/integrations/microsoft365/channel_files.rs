//! Selection-scoped Microsoft Graph channel-files metadata normalization.
//!
//! A Teams channel's files live behind a channel-specific SharePoint folder,
//! not necessarily the signed-in user's OneDrive. This adapter resolves that
//! folder first, then reads one bounded page of its direct children. The
//! backing drive/folder identifiers, raw DTOs, cursors, paths, and content
//! URLs never cross this integration boundary. An optional, validated browser
//! display URL may cross as an opaque `ProviderBrowserUrl` for an explicit
//! user-initiated system-browser handoff.

use super::GraphSession;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderBrowserUrl, ProviderResourceRef, RemoteChannelFilesProjection, RemoteDriveItem,
    RemoteDriveItemKind,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

const MAX_CHANNEL_FILES_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_CHANNEL_FILES: usize = 50;
const MAX_ITEM_NAME_BYTES: usize = 512;
const MAX_TIMESTAMP_BYTES: usize = 96;

/// Direct-child metadata and presentation-safe notices. The resolved backing
/// folder is intentionally discarded before this result is constructed.
struct ChannelFilesReadResult {
    items: Vec<RemoteDriveItem>,
    notices: Vec<String>,
}

/// Categorical failures prevent Graph error bodies and route details from
/// being rendered, persisted, or surfaced in a toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelFilesReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl ChannelFilesReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph channel files synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one selected channel's backing-folder direct children. The
    /// selection, rather than the whole tenant hierarchy, defines the only
    /// route this optional read may access.
    pub(super) fn poll_channel_files(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_files_with_cancellation(team, channel, None)
    }

    /// Lets the serial worker stop a stale selected-channel file read before
    /// it can publish a projection for another selection or account.
    pub(super) fn poll_channel_files_cancellable(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_files_with_cancellation(team, channel, Some(cancellation))
    }

    fn poll_channel_files_with_cancellation(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let (team_id, channel_id) = selected_channel_identifiers(&team, &channel)?;
        let completed = SyncInterest::ChannelFiles {
            team: team.clone(),
            channel: channel.clone(),
        };
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }

        match fetch_channel_files(
            &self.session,
            team_id,
            channel_id,
            cancellation,
        ) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: Some(RemoteChannelFilesProjection::microsoft365(
                    team,
                    channel,
                    result.items,
                )?),
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(ChannelFilesReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Preserve the selected channel while clearing old metadata.
                // A 403 can be a consent or membership condition, so keep the
                // feature-facing state neutral about its precise cause.
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: Some(RemoteChannelFilesProjection::access_denied(
                    team, channel,
                )?),
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this channel's files. Verify a documented delegated file permission (Files.Read.All is the documented least-privilege channel-files scope) and your channel membership, then reconnect."
                        .into(),
                ],
            }),
            Err(ChannelFilesReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(ChannelFilesReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn selected_channel_identifiers<'a>(
    team: &'a ProviderResourceRef,
    channel: &'a ProviderResourceRef,
) -> Result<(&'a str, &'a str), TransportError> {
    let team_id = team.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
    })?;
    let channel_id = channel.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview channel".into())
    })?;
    if normalized_identifier(team_id).as_deref() != Some(team_id)
        || normalized_identifier(channel_id).as_deref() != Some(channel_id)
    {
        return Err(TransportError::InvalidCommand(
            "Microsoft Graph channel file selection contains an invalid provider identifier".into(),
        ));
    }
    Ok((team_id, channel_id))
}

fn fetch_channel_files(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChannelFilesReadResult, ChannelFilesReadFailure> {
    check_cancelled(cancellation)?;
    let (drive_id, folder_id) =
        resolve_channel_files_folder(session, team_id, channel_id, cancellation)?;
    // The folder IDs are adapter-local route material only. Check cancellation
    // before turning them into the direct-children request.
    check_cancelled(cancellation)?;
    let (items, truncated, mut skipped_items) =
        fetch_folder_children(session, &drive_id, &folder_id, cancellation)?;
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
            "Microsoft 365 channel file sync is showing at most {MAX_CHANNEL_FILES} direct items."
        ));
    }
    if skipped_items > 0 {
        notices.push(
            "Microsoft Graph returned some channel files without usable identifiers or names; they were skipped."
                .into(),
        );
    }
    check_cancelled(cancellation)?;
    Ok(ChannelFilesReadResult {
        items: normalized,
        notices,
    })
}

/// Resolves only the selected channel's backing files folder. Microsoft Graph
/// documents no OData query options for this route, so it receives no query
/// string and no future caller may add one here.
fn resolve_channel_files_folder(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<(String, String), ChannelFilesReadFailure> {
    let url = channel_files_folder_url(session.cloud(), team_id, channel_id)?;
    let response = get_json(session, &url, cancellation)?;
    let folder = serde_json::from_slice::<GraphChannelFilesFolder>(&response.body)
        .map_err(|_| ChannelFilesReadFailure::Unavailable)?;
    let folder_id = folder
        .id
        .as_deref()
        .and_then(normalized_identifier)
        .ok_or(ChannelFilesReadFailure::Unavailable)?;
    let drive_id = folder
        .parent_reference
        .as_ref()
        .and_then(|parent| parent.drive_id.as_deref())
        .and_then(normalized_identifier)
        .ok_or(ChannelFilesReadFailure::Unavailable)?;
    // A `filesFolder` response must represent a container. Do not use a
    // malformed file-like response as an arbitrary children route.
    folder
        .folder
        .as_ref()
        .is_some_and(Value::is_object)
        .then_some((drive_id, folder_id))
        .ok_or(ChannelFilesReadFailure::Unavailable)
}

/// Performs exactly one direct-children request. An opaque provider cursor is
/// observed only to report truncation; it is never parsed or followed.
fn fetch_folder_children(
    session: &GraphSession<'_>,
    drive_id: &str,
    folder_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphDriveItem>, bool, usize), ChannelFilesReadFailure> {
    let url = channel_files_children_url(session.cloud(), drive_id, folder_id)?;
    let response = get_json(session, &url, cancellation)?;
    let collection = decode_collection(&response.body)?;
    let mut items = Vec::with_capacity(collection.value.len().min(MAX_CHANNEL_FILES));
    let mut seen = BTreeSet::new();
    let mut skipped_items = 0_usize;
    let truncated = collection.next_link.is_some() || collection.value.len() > MAX_CHANNEL_FILES;

    for item in collection.value.into_iter().take(MAX_CHANNEL_FILES) {
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

fn channel_files_folder_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
) -> Result<Url, ChannelFilesReadFailure> {
    let url = cloud
        .graph_route(
            &["teams", team_id, "channels", channel_id, "filesFolder"],
            None,
        )
        .map_err(|_| ChannelFilesReadFailure::Unavailable)?;
    Ok(url)
}

fn channel_files_children_url(
    cloud: GraphCloud,
    drive_id: &str,
    folder_id: &str,
) -> Result<Url, ChannelFilesReadFailure> {
    let mut url = cloud
        .graph_route(&["drives", drive_id, "items", folder_id, "children"], None)
        .map_err(|_| ChannelFilesReadFailure::Unavailable)?;
    // Keep the direct-child response metadata-only and bounded. No `$expand`,
    // recursion, content route, or continuation belongs to this initial slice.
    url.query_pairs_mut()
        .append_pair(
            "$select",
            "id,name,lastModifiedDateTime,size,file,folder,package,webUrl",
        )
        .append_pair("$top", &MAX_CHANNEL_FILES.to_string());
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
    let kind = classify_drive_item_kind(
        item.file.as_ref(),
        item.folder.as_ref(),
        item.package.as_ref(),
    );
    let web_url = item
        .web_url
        .as_deref()
        .and_then(ProviderBrowserUrl::microsoft365);
    let target = ProviderResourceRef::microsoft365(id);
    match web_url {
        Some(web_url) => RemoteDriveItem::microsoft365_with_web_url(
            target,
            name,
            modified_at,
            size_bytes,
            kind,
            Some(web_url),
        ),
        None => RemoteDriveItem::microsoft365(target, name, modified_at, size_bytes, kind),
    }
    .ok()
}

fn classify_drive_item_kind(
    file: Option<&Value>,
    folder: Option<&Value>,
    package: Option<&Value>,
) -> RemoteDriveItemKind {
    match (
        file.is_some_and(Value::is_object),
        folder.is_some_and(Value::is_object),
        package.is_some_and(Value::is_object),
    ) {
        (true, false, false) => RemoteDriveItemKind::File,
        (false, true, false) => RemoteDriveItemKind::Folder,
        // A drive package has provider-specific semantics. Do not treat it as
        // a normal downloadable file or browsable folder in this metadata-only
        // view.
        _ => RemoteDriveItemKind::Unknown,
    }
}

fn get_json(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphHttpResponse, ChannelFilesReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_CHANNEL_FILES_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_response_status(&response)?;
    Ok(response)
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), ChannelFilesReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(ChannelFilesReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> ChannelFilesReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => ChannelFilesReadFailure::Unavailable,
    }
}

fn validate_response_status(response: &GraphHttpResponse) -> Result<(), ChannelFilesReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(ChannelFilesReadFailure::ReauthenticationRequired),
        403 => Err(ChannelFilesReadFailure::AccessDenied),
        429 => Err(ChannelFilesReadFailure::Throttled(
            response.retry_after_seconds,
        )),
        _ => Err(ChannelFilesReadFailure::Unavailable),
    }
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection(body: &[u8]) -> Result<GraphCollection, ChannelFilesReadFailure> {
    serde_json::from_slice::<GraphCollection>(body)
        .map_err(|_| ChannelFilesReadFailure::Unavailable)
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
struct GraphChannelFilesFolder {
    id: Option<String>,
    parent_reference: Option<GraphParentReference>,
    folder: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphParentReference {
    drive_id: Option<String>,
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
    package: Option<Value>,
    web_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelFilesReadFailure, GraphHttpError, GraphHttpResponse,
        MAX_CHANNEL_FILES_RESPONSE_BYTES, Microsoft365Transport, RemoteDriveItemKind, SyncInterest,
        TransportCancellation, channel_files_children_url, channel_files_folder_url,
        fetch_channel_files, selected_channel_identifiers,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, ProviderBrowserUrl, ProviderResourceRef, TransportError,
        WorkspaceTransport,
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
        requested_body_limits: Mutex<Vec<u64>>,
    }

    impl TestHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
                requested_body_limits: Mutex::new(Vec::new()),
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
            max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .push(url.as_str().into());
            self.requested_body_limits
                .lock()
                .expect("test response-limit list must not be poisoned")
                .push(max_response_bytes);
            self.responses
                .lock()
                .expect("test response queue must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    struct CancellingAfterFolderClient {
        cancellation: TransportCancellation,
        requests: AtomicUsize,
    }

    impl GraphHttpClient for CancellingAfterFolderClient {
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
            response(
                200,
                r#"{"id":"folder-1","parentReference":{"driveId":"drive-1"},"folder":{}}"#,
            )
        }
    }

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn response_with_retry(
        status: u16,
        retry_after_seconds: Option<u64>,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds,
            body: Vec::new(),
        })
    }

    fn selection() -> (ProviderResourceRef, ProviderResourceRef) {
        (
            ProviderResourceRef::microsoft365("team-1"),
            ProviderResourceRef::microsoft365("channel-1"),
        )
    }

    #[test]
    fn channel_files_use_the_documented_two_step_routes_and_safe_metadata() {
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"id":"folder-1","parentReference":{"driveId":"drive-1"},"folder":{}}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":"file-1","name":"  Release\nplan.docx ","lastModifiedDateTime":"2026-11-06T10:30:00Z","size":42,"file":{},"webUrl":" https://contoso.sharepoint.com/:w:/r/sites/Team/Shared%20Documents/Release%20plan.docx "},{"id":"folder-2","name":"Design","lastModifiedDateTime":"2026-11-06T09:00:00Z","size":0,"folder":{},"webUrl":"http://contoso.sharepoint.com/sites/Team/Design"},{"id":"package-3","name":"Notebook","lastModifiedDateTime":"2026-11-06T08:00:00Z","package":{}}]}"#,
            ),
        ]);

        let result = fetch_channel_files(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            None,
        )
        .expect("valid channel files must normalize");

        assert_eq!(result.items.len(), 3);
        assert_eq!(result.items[0].name(), "Release plan.docx");
        assert_eq!(result.items[0].size_bytes(), Some(42));
        assert_eq!(result.items[0].kind(), RemoteDriveItemKind::File);
        assert_eq!(
            result.items[0]
                .web_url()
                .map(<ProviderBrowserUrl as AsRef<str>>::as_ref),
            Some(
                "https://contoso.sharepoint.com/:w:/r/sites/Team/Shared%20Documents/Release%20plan.docx"
            )
        );
        assert_eq!(result.items[1].kind(), RemoteDriveItemKind::Folder);
        assert_eq!(result.items[1].web_url(), None);
        assert_eq!(result.items[2].kind(), RemoteDriveItemKind::Unknown);
        assert!(result.notices.is_empty());

        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested.len(), 2);
        let folder_url = Url::parse(&requested[0]).expect("folder request must be a URL");
        assert_eq!(
            folder_url.path(),
            "/v1.0/teams/team-1/channels/channel-1/filesFolder"
        );
        assert!(folder_url.query().is_none());
        let children_url = Url::parse(&requested[1]).expect("children request must be a URL");
        assert_eq!(
            children_url.path(),
            "/v1.0/drives/drive-1/items/folder-1/children"
        );
        let query = children_url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.len(), 2);
        assert_eq!(query[0].0, "$select");
        assert_eq!(
            query[0].1,
            "id,name,lastModifiedDateTime,size,file,folder,package,webUrl"
        );
        assert_eq!(query[1].0, "$top");
        assert_eq!(query[1].1, "50");
        assert_eq!(
            client
                .requested_body_limits
                .lock()
                .expect("test response-limit list must not be poisoned")
                .as_slice(),
            [
                MAX_CHANNEL_FILES_RESPONSE_BYTES,
                MAX_CHANNEL_FILES_RESPONSE_BYTES
            ]
        );
    }

    #[test]
    fn malformed_channel_file_browser_urls_are_dropped_without_dropping_metadata() {
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
                file: Some(serde_json::json!({})),
                folder: None,
                package: None,
                web_url: Some(value.into()),
            })
            .expect("an invalid optional browser URL must not reject valid metadata");
            assert_eq!(item.web_url(), None);
        }
    }

    #[test]
    fn channel_file_children_stop_at_one_page_and_report_truncation() {
        let values = (0..51)
            .map(|index| {
                format!(
                    r#"{{"id":"item-{index}","name":"Item {index}","lastModifiedDateTime":"2026-11-06T10:30:00Z","file":{{}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"id":"folder-1","parentReference":{"driveId":"drive-1"},"folder":{}}"#,
            ),
            response(
                200,
                &format!(
                    r#"{{"value":[{values}],"@odata.nextLink":"https://example.test/not-followed"}}"#
                ),
            ),
        ]);

        let result = fetch_channel_files(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            None,
        )
        .expect("the first channel-files page must normalize");

        assert_eq!(result.items.len(), 50);
        assert!(result.notices[0].contains("at most 50 direct items"));
        assert_eq!(
            client
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            2
        );
    }

    #[test]
    fn malformed_folder_drive_or_items_are_not_used_as_routes() {
        let missing_folder = TestHttpClient::new([response(
            200,
            r#"{"id":"folder-1","parentReference":{"driveId":"drive-1"}}"#,
        )]);
        assert!(matches!(
            fetch_channel_files(
                &GraphSession::for_test(&missing_folder),
                "team-1",
                "channel-1",
                None
            ),
            Err(ChannelFilesReadFailure::Unavailable)
        ));
        assert_eq!(
            missing_folder
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            1
        );

        let missing_drive =
            TestHttpClient::new([response(200, r#"{"id":"folder-1","folder":{}}"#)]);
        assert!(matches!(
            fetch_channel_files(
                &GraphSession::for_test(&missing_drive),
                "team-1",
                "channel-1",
                None
            ),
            Err(ChannelFilesReadFailure::Unavailable)
        ));
        assert_eq!(
            missing_drive
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            1
        );

        let malformed_drive = TestHttpClient::new([response(
            200,
            r#"{"id":"folder-1","parentReference":{"driveId":"\u0000"},"folder":{}}"#,
        )]);
        assert!(matches!(
            fetch_channel_files(
                &GraphSession::for_test(&malformed_drive),
                "team-1",
                "channel-1",
                None
            ),
            Err(ChannelFilesReadFailure::Unavailable)
        ));
        assert_eq!(
            malformed_drive
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            1
        );

        let malformed_items = TestHttpClient::new([
            response(
                200,
                r#"{"id":"folder-1","parentReference":{"driveId":"drive-1"},"folder":{}}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":" ","name":"Missing ID","file":{}},{"id":"valid-id","name":"\u0000","file":{}},{"id":"valid-item","name":"Visible","lastModifiedDateTime":"\u0000","file":{}}]}"#,
            ),
        ]);
        let result = fetch_channel_files(
            &GraphSession::for_test(&malformed_items),
            "team-1",
            "channel-1",
            None,
        )
        .expect("malformed rows must not fail the bounded listing");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].name(), "Visible");
        assert_eq!(result.items[0].modified_at(), "Modified time unavailable");
        assert!(result.notices[0].contains("without usable identifiers or names"));
    }

    #[test]
    fn cancellation_between_folder_and_children_requests_discards_the_read() {
        let cancellation = TransportCancellation::new();
        let client = CancellingAfterFolderClient {
            cancellation: cancellation.clone(),
            requests: AtomicUsize::new(0),
        };

        let result = fetch_channel_files(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            Some(&cancellation),
        );

        assert!(matches!(result, Err(ChannelFilesReadFailure::Cancelled)));
        assert_eq!(client.requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn access_denial_replaces_channel_files_without_losing_verified_identity() {
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
        let (team, channel) = selection();

        let delta = transport
            .poll(SyncInterest::ChannelFiles {
                team: team.clone(),
                channel: channel.clone(),
            })
            .expect("channel file access denial must retain a verified identity");

        let projection = delta
            .remote_channel_files
            .expect("an access-denied channel-files projection is expected");
        assert!(projection.is_access_denied());
        assert_eq!(projection.team(), team);
        assert_eq!(projection.channel(), channel);
        assert!(projection.items().is_empty());
        assert!(delta.notices[0].contains("Files.Read.All"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(current) if current.account_id == "u1"
        ));
    }

    #[test]
    fn reauthentication_clears_the_verified_identity() {
        let client =
            Box::new(TestHttpClient::new([response(401, "{}")])) as Box<dyn GraphHttpClient>;
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity);
        let (team, channel) = selection();

        assert!(matches!(
            transport.poll(SyncInterest::ChannelFiles { team, channel }),
            Err(TransportError::ReauthenticationRequired)
        ));
        assert!(transport.identity.is_none());
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::ReauthenticationRequired
        ));
    }

    #[test]
    fn throttling_preserves_retry_metadata() {
        let client = TestHttpClient::new([response_with_retry(429, Some(17))]);

        assert!(matches!(
            fetch_channel_files(
                &GraphSession::for_test(&client),
                "team-1",
                "channel-1",
                None
            ),
            Err(ChannelFilesReadFailure::Throttled(Some(17)))
        ));
    }

    #[test]
    fn local_preview_targets_are_rejected_before_networking() {
        let (_, channel) = selection();
        let local_team = ProviderResourceRef::local_preview("team-1");

        assert!(selected_channel_identifiers(&local_team, &channel).is_err());
    }

    #[test]
    fn built_in_channel_file_urls_are_valid() {
        assert!(channel_files_folder_url(GraphCloud::Global, "team-1", "channel-1").is_ok());
        assert!(channel_files_children_url(GraphCloud::Global, "drive-1", "folder-1").is_ok());
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            for route in [
                super::channel_files_folder_url(cloud, "t", "c"),
                super::channel_files_children_url(cloud, "d", "f"),
            ] {
                let url = route.unwrap_or_else(|_| panic!("route must build"));
                assert_eq!(url.scheme(), "https");
                assert_eq!(url.host_str(), Some(cloud.graph_host()));
                assert!(url.path().starts_with("/v1.0/"));
            }
        }
    }
}

#[cfg(test)]
mod decode_collection_tests {
    use super::{ChannelFilesReadFailure, decode_collection};

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
                Err(ChannelFilesReadFailure::Unavailable)
            ));
        }
    }
}
