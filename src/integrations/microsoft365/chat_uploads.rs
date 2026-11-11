//! Chat file upload to the signed-in user's OneDrive.
//!
//! One picked local file of at most 4 MiB goes to
//! `me/drive/root:/Microsoft Teams Chat Files/{name}:/content` with a
//! never-overwrite policy, so a file the user already has is never replaced:
//!
//! 1. one `GET` of the exact path must answer `404` (a `200` is a name
//!    conflict);
//! 2. one `PUT` with `?@microsoft.graph.conflictBehavior=fail` (a `409` is a
//!    name conflict);
//! 3. one `GET` of the exact path with `$select` for the `eTag` (whose GUID is
//!    the attachment ID) and the `webDavUrl` (the attachment's `contentUrl`),
//!    which a `PUT` answer does not carry.
//!
//! The `PUT` is a write and is never replayed or retried. After a network
//! error or a `5xx` the outcome is unknown, so exactly one `GET` of the same
//! path looks for the file: if it is there with the expected size the handle
//! comes back marked recovered and the user decides whether to attach it;
//! otherwise the upload is reported uncertain. Nothing is re-uploaded.
//!
//! The picked path, the drive item, its URLs and ETag never leave this module:
//! the feature receives an opaque [`UploadedFileHandle`]. See Graph's
//! [upload small files](https://learn.microsoft.com/en-us/graph/api/driveitem-put-content)
//! and the `@microsoft.graph.conflictBehavior` instance attribute on the
//! [driveItem resource](https://learn.microsoft.com/en-us/graph/api/resources/driveitem).

use super::chats::selected_chat_identifier;
use super::content_hash::{ContentHashes, HashVerdict, ReportedHashes};
use super::file_preview::preauthorized_content_url;
use super::mutation::map_mutation_http_error;
use super::{GraphSession, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    CommandDisposition, LocalUploadSource, MAX_CHAT_UPLOAD_BYTES, UPLOAD_NAME_CONFLICT,
    UploadReceipt, UploadedFileHandle, validate_upload_name,
};
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use serde_json::Value;
use std::io::Read;
use std::time::SystemTime;
use url::Url;

/// The folder Teams itself uses for files shared in chats.
pub(super) const CHAT_FILES_FOLDER: &str = "Microsoft Teams Chat Files";
pub(super) const ITEM_SELECT: &str = "$select=id,name,size,eTag,webDavUrl,file";
const CONFLICT_BEHAVIOR_FAIL: &str = "@microsoft.graph.conflictBehavior=fail";
pub(super) const MAX_UPLOAD_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_REMEMBERED_UPLOADS: usize = 16;
const MAX_ITEM_ID_BYTES: usize = 256;

pub(super) const CHANGED_MESSAGE: &str = "The file changed after you picked it. Attach it again.";
pub(super) const UNCERTAIN_MESSAGE: &str = "Microsoft Graph could not confirm the file upload";
pub(super) const UNAVAILABLE_MESSAGE: &str = "Microsoft Graph file upload is unavailable";

/// What the adapter keeps for one uploaded file, private to this module.
struct RegisteredUpload {
    id: u64,
    /// The drive item's ID, used only to share the file with a chat.
    item_id: String,
    reference: String,
    content_url: String,
    name: String,
}

/// The adapter's bounded registry of uploaded files, keyed by the opaque
/// handle ID it issued. It is cleared on reauthentication and never logged:
/// `Debug` shows a count.
#[derive(Default)]
pub(super) struct UploadRegistry {
    next_id: u64,
    entries: Vec<RegisteredUpload>,
}

impl std::fmt::Debug for UploadRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadRegistry")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl UploadRegistry {
    /// Handle IDs only ever grow, starting at a random point per adapter, so
    /// a handle held across a clear or an adapter replacement never matches a
    /// different file.
    fn allocate_id(&mut self) -> u64 {
        if self.next_id == 0 {
            let mut bytes = [0_u8; 6];
            let _ = getrandom::fill(&mut bytes);
            self.next_id = bytes
                .iter()
                .fold(1_u64, |sum, byte| (sum << 8) | u64::from(*byte));
        }
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    pub(super) fn register_item(
        &mut self,
        name: String,
        reference: String,
        content_url: String,
        item_id: String,
    ) -> u64 {
        let id = self.allocate_id();
        if self.entries.len() >= MAX_REMEMBERED_UPLOADS {
            self.entries.remove(0);
        }
        self.entries.push(RegisteredUpload {
            id,
            item_id,
            reference,
            content_url,
            name,
        });
        id
    }

    /// A registration with a placeholder item ID, for tests that never share.
    #[cfg(test)]
    pub(super) fn register(&mut self, name: String, reference: String, content_url: String) -> u64 {
        self.register_item(name, reference, content_url, "item-1".into())
    }

    /// The drive item of an uploaded file this session issued the handle for.
    pub(super) fn item_id(&self, handle: &UploadedFileHandle) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.id == handle.id() && entry.reference == handle.reference_id())
            .map(|entry| entry.item_id.as_str())
    }

    /// The registered file for `handle`, only if its reference still matches.
    pub(super) fn resolve(&self, handle: &UploadedFileHandle) -> Option<(&str, &str, &str)> {
        self.entries
            .iter()
            .find(|entry| entry.id == handle.id() && entry.reference == handle.reference_id())
            .map(|entry| {
                (
                    entry.reference.as_str(),
                    entry.content_url.as_str(),
                    entry.name.as_str(),
                )
            })
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A drive item as read back from Graph, reduced to what an attachment needs.
pub(super) struct ItemInfo {
    /// The drive item's own ID, which must equal the ID the `PUT` answered
    /// with before the file is trusted to be that upload.
    pub(super) id: String,
    pub(super) size: u64,
    pub(super) hashes: ReportedHashes,
    pub(super) reference: String,
    pub(super) content_url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum UploadFailure {
    ReauthenticationRequired,
    Forbidden,
    Throttled(Option<u64>),
    Unavailable,
}

impl UploadFailure {
    pub(super) fn transport_error(self) -> TransportError {
        match self {
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::Forbidden => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()),
        }
    }
}

impl Microsoft365Transport {
    pub(super) fn upload_chat_file(
        &mut self,
        chat: &ProviderResourceRef,
        source: &LocalUploadSource,
    ) -> Result<CommandReceipt, TransportError> {
        // The chat is only validated: the file goes to the user's own drive.
        selected_chat_identifier(chat)?;
        validate_upload_name(source.name())
            .map_err(|error| TransportError::InvalidCommand(error.user_message().into()))?;
        if source.size() > MAX_CHAT_UPLOAD_BYTES {
            return self.start_upload_session(chat, source);
        }
        let bytes = read_source(source)?;
        let local = ContentHashes::of_bytes(&bytes);

        let item_url = item_url(self.session.cloud(), source.name(), Some(ITEM_SELECT))?;
        // Step 1: the name must be free.
        match name_is_taken(&self.session, &item_url) {
            Ok(true) => {
                return Err(TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into()));
            }
            Ok(false) => {}
            Err(failure) => return Err(self.upload_failure(failure)),
        }

        // Step 2: the one PUT. Never replayed.
        let put_url = content_url(self.session.cloud(), source.name())?;
        let response = self
            .session
            .put_bytes_once(&put_url, "application/octet-stream", &bytes)
            .map_err(|error| {
                map_mutation_http_error(error, UNCERTAIN_MESSAGE, UNAVAILABLE_MESSAGE)
            });
        drop(bytes);
        // The item the PUT answered with, when it did. Without it the file can
        // only be recovered on an exact hash match.
        let answered_id = match response {
            Ok(response) => match response.status {
                200 | 201 => answered_item_id(&response.body),
                409 => {
                    return Err(TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into()));
                }
                413 | 507 => {
                    return Err(TransportError::InvalidCommand(
                        "OneDrive has no room for this file or refused its size".into(),
                    ));
                }
                401 => {
                    let error = TransportError::ReauthenticationRequired;
                    self.identity = None;
                    self.record_error(&error);
                    return Err(error);
                }
                403 => return Err(TransportError::Unauthorized),
                // The PUT was refused and is never replayed, even then: the
                // draft and the picked file stay for the user to try again.
                429 => {
                    return Err(TransportError::Throttled {
                        retry_after_seconds: response.retry_after_seconds,
                    });
                }
                400 | 404 | 422 => {
                    return Err(TransportError::InvalidCommand(
                        "OneDrive rejected the file".into(),
                    ));
                }
                _ => None,
            },
            Err(TransportError::Unavailable(_)) => None,
            Err(error) => return Err(error),
        };

        // Step 3 (or the one look after an unknown outcome): read the file
        // back by its exact path.
        let found = match get_item(&self.session, &item_url) {
            Ok(found) => found,
            Err(UploadFailure::ReauthenticationRequired) => {
                return Err(self.upload_failure(UploadFailure::ReauthenticationRequired));
            }
            Err(_) => None,
        };
        let Some(info) = found else {
            return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
        };
        let recovered = match judge_read_back(&info, answered_id.as_deref(), source.size(), &local)
        {
            ReadBack::Verified => false,
            ReadBack::Recovered => true,
            // Something with this name exists, but it is not demonstrably this
            // upload.
            ReadBack::Unproven => {
                return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
            }
        };
        let id = self.uploads.register_item(
            source.name().to_owned(),
            info.reference.clone(),
            info.content_url,
            info.id,
        );
        Ok(CommandReceipt {
            command_id: "microsoft365:chat-upload".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(UploadReceipt::Done(UploadedFileHandle::new(
                id,
                source.name().to_owned(),
                source.size(),
                info.reference,
                recovered,
            ))),
        })
    }

    pub(super) fn upload_failure(&mut self, failure: UploadFailure) -> TransportError {
        if failure == UploadFailure::ReauthenticationRequired {
            let error = TransportError::ReauthenticationRequired;
            self.identity = None;
            self.record_error(&error);
            return error;
        }
        failure.transport_error()
    }
}

/// Opens the picked file, refusing it if it is not the file that was picked.
///
/// On unix it is opened with `O_NONBLOCK | O_NOFOLLOW`, so a FIFO or device
/// swapped in after the pick cannot block the worker and a symlink is never
/// followed, and the opened handle (not the path) is then checked: a regular
/// file, the size and modified time of the pick, and, on unix, the same device
/// and inode.
pub(super) fn open_unchanged(source: &LocalUploadSource) -> Result<std::fs::File, TransportError> {
    open_picked(
        source.path(),
        source.size(),
        source.modified(),
        source.identity(),
    )
}

pub(super) fn open_picked(
    path: &std::path::Path,
    size: u64,
    modified: Option<SystemTime>,
    identity: Option<(u64, u64)>,
) -> Result<std::fs::File, TransportError> {
    let changed = || TransportError::InvalidCommand(CHANGED_MESSAGE.into());
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|_| changed())?;
    let metadata = file.metadata().map_err(|_| changed())?;
    let unchanged_time = match (modified, metadata.modified().ok()) {
        (Some(picked), Some(now)) => same_instant(picked, now),
        _ => true,
    };
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        identity.is_none_or(|(device, inode)| metadata.dev() == device && metadata.ino() == inode)
    };
    #[cfg(not(unix))]
    let same_file = {
        let _ = identity;
        true
    };
    if !metadata.is_file() || metadata.len() != size || !unchanged_time || !same_file {
        return Err(changed());
    }
    Ok(file)
}

/// Reads the whole of a small picked file.
fn read_source(source: &LocalUploadSource) -> Result<Vec<u8>, TransportError> {
    let changed = || TransportError::InvalidCommand(CHANGED_MESSAGE.into());
    if source.size() > MAX_CHAT_UPLOAD_BYTES {
        return Err(TransportError::InvalidCommand(
            "this file needs an upload session".into(),
        ));
    }
    let file = open_unchanged(source)?;
    let mut bytes = Vec::with_capacity(usize::try_from(source.size()).unwrap_or(0));
    file.take(MAX_CHAT_UPLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| changed())?;
    if bytes.len() as u64 != source.size() {
        return Err(changed());
    }
    Ok(bytes)
}

fn same_instant(left: SystemTime, right: SystemTime) -> bool {
    left == right
}

/// `me/drive/root:/Microsoft Teams Chat Files/{name}` (the path form of an
/// item), optionally with a `$select`. Segments are percent-encoded by the
/// route builder; the name has already been validated, so it can carry no
/// separator or dot segment.
pub(super) fn item_url(
    cloud: GraphCloud,
    name: &str,
    query: Option<&str>,
) -> Result<Url, TransportError> {
    cloud
        .graph_route(&["me", "drive", "root:", CHAT_FILES_FOLDER, name], query)
        .map_err(|_| route_error())
}

/// `me/drive/root:/Microsoft Teams Chat Files/{name}:/content?conflict=fail`.
fn content_url(cloud: GraphCloud, name: &str) -> Result<Url, TransportError> {
    cloud
        .graph_route(
            &[
                "me",
                "drive",
                "root:",
                CHAT_FILES_FOLDER,
                &format!("{name}:"),
                "content",
            ],
            Some(CONFLICT_BEHAVIOR_FAIL),
        )
        .map_err(|_| route_error())
}

fn route_error() -> TransportError {
    TransportError::Unavailable("could not prepare the Microsoft Graph upload route".into())
}

/// Whether anything is at this exact path. Any `200` is a taken name, however
/// the item looks; only a `404` frees it.
pub(super) fn name_is_taken(session: &GraphSession<'_>, url: &Url) -> Result<bool, UploadFailure> {
    let response = session
        .get_json(url, MAX_UPLOAD_RESPONSE_BYTES)
        .map_err(|_| UploadFailure::Unavailable)?;
    match response.status {
        200 => Ok(true),
        404 => Ok(false),
        401 => Err(UploadFailure::ReauthenticationRequired),
        403 => Err(UploadFailure::Forbidden),
        429 => Err(UploadFailure::Throttled(response.retry_after_seconds)),
        _ => Err(UploadFailure::Unavailable),
    }
}

/// One read of an item by exact path. `404` is `None`.
pub(super) fn get_item(
    session: &GraphSession<'_>,
    url: &Url,
) -> Result<Option<ItemInfo>, UploadFailure> {
    let response = session
        .get_json(url, MAX_UPLOAD_RESPONSE_BYTES)
        .map_err(|_| UploadFailure::Unavailable)?;
    match response.status {
        200 => {}
        404 => return Ok(None),
        401 => return Err(UploadFailure::ReauthenticationRequired),
        403 => return Err(UploadFailure::Forbidden),
        429 => return Err(UploadFailure::Throttled(response.retry_after_seconds)),
        _ => return Err(UploadFailure::Unavailable),
    }
    let value =
        serde_json::from_slice::<Value>(&response.body).map_err(|_| UploadFailure::Unavailable)?;
    // An item without a usable ETag GUID or WebDAV URL cannot be attached.
    let reference = value
        .get("eTag")
        .and_then(Value::as_str)
        .and_then(etag_guid)
        .ok_or(UploadFailure::Unavailable)?;
    let content_url = value
        .get("webDavUrl")
        .and_then(Value::as_str)
        .and_then(|value| content_url_on_this_cloud(session.cloud(), value))
        .ok_or(UploadFailure::Unavailable)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .and_then(super::text::normalized_identifier)
        .filter(|id| id.len() <= MAX_ITEM_ID_BYTES)
        .ok_or(UploadFailure::Unavailable)?;
    let reported = |name: &str| {
        value
            .pointer(&format!("/file/hashes/{name}"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|hash| !hash.is_empty() && hash.len() <= 128)
            .map(str::to_owned)
    };
    let hashes = ReportedHashes {
        quick_xor: reported("quickXorHash"),
        sha256: reported("sha256Hash"),
    };
    let size = value
        .get("size")
        .and_then(Value::as_u64)
        .ok_or(UploadFailure::Unavailable)?;
    Ok(Some(ItemInfo {
        id,
        hashes,
        size,
        reference,
        content_url,
    }))
}

/// The attachment ID: the GUID inside an ETag such as `"{GUID},3"`, as a
/// lowercase hyphenated string. Anything else yields `None`.
fn etag_guid(etag: &str) -> Option<String> {
    let start = etag.find('{')? + 1;
    let end = etag[start..].find('}')? + start;
    let guid = etag[start..end].to_ascii_lowercase();
    super::rich_html::is_attachment_guid(&guid).then_some(guid)
}

/// The attachment's `contentUrl`: HTTPS on a subdomain of this cloud's content
/// hosts (the label-boundary check the preview hop uses), with no credentials,
/// port or fragment.
fn content_url_on_this_cloud(cloud: GraphCloud, value: &str) -> Option<String> {
    let url = preauthorized_content_url(cloud, value)?;
    url.port().is_none().then(|| url.to_string())
}

/// How a file read back from OneDrive relates to the upload.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReadBack {
    /// It is this upload: the `PUT` answered with this very item.
    Verified,
    /// The `PUT`'s answer was lost, but the bytes are provably the ones that
    /// were picked (an exact hash match). The user decides.
    Recovered,
    /// Not demonstrably this upload.
    Unproven,
}

/// Decides whether a read-back item is the upload:
///
/// * the size must always equal the picked file's;
/// * a reported hash must never differ from the local bytes' hash;
/// * with the item ID the `PUT` answered with, the read-back must have that
///   same ID, and the file is verified;
/// * without it, only an exact, reported hash match can recover the file, and a
///   file that reports no comparable hash is never adopted.
pub(super) fn judge_read_back(
    info: &ItemInfo,
    answered_id: Option<&str>,
    size: u64,
    local: &ContentHashes,
) -> ReadBack {
    if info.size != size {
        return ReadBack::Unproven;
    }
    let verdict = info.hashes.compare(local);
    if verdict == HashVerdict::Differs {
        return ReadBack::Unproven;
    }
    match answered_id {
        Some(id) if info.id == id => ReadBack::Verified,
        Some(_) => ReadBack::Unproven,
        None if verdict == HashVerdict::Match => ReadBack::Recovered,
        None => ReadBack::Unproven,
    }
}

/// The item ID in a `PUT` or final-fragment answer, if it has a usable one.
pub(super) fn answered_item_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("id")?
        .as_str()
        .and_then(super::text::normalized_identifier)
        .filter(|id| id.len() <= MAX_ITEM_ID_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphHttpError, GraphHttpResponse};
    use crate::integrations::transport::{AccountIdentity, ConnectionStatus};
    use crate::integrations::{WorkspaceCommand, WorkspaceTransport};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";

    #[derive(Default)]
    struct State {
        gets: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        puts: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        /// Every request in order: "GET <url>" or "PUT <url> <len> <type>".
        log: Mutex<Vec<String>>,
        put_bodies: Mutex<Vec<Vec<u8>>>,
    }

    struct Client(Arc<State>);

    impl GraphHttpClient for Client {
        fn get_me(&self, _: &Url, _: &str) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            _: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0.log.lock().unwrap().push(format!("GET {url}"));
            self.0
                .gets
                .lock()
                .unwrap()
                .pop_front()
                .expect("get scripted")
        }

        fn put_bytes(
            &self,
            url: &Url,
            content_type: &str,
            body: &[u8],
            _: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .log
                .lock()
                .unwrap()
                .push(format!("PUT {url} {} {content_type}", body.len()));
            self.0.put_bodies.lock().unwrap().push(body.to_vec());
            self.0
                .puts
                .lock()
                .unwrap()
                .pop_front()
                .expect("put scripted")
        }
    }

    fn resp(status: u16, body: Value) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.to_string().into_bytes(),
        })
    }

    fn item_json(size: u64, id: &str, host: &str, hashes: Option<(&str, &str)>) -> Value {
        let mut item = json!({"id": id, "name": "q3.pdf", "size": size,
               "eTag": format!("\"{{{}}},2\"", GUID.to_ascii_uppercase()),
               "webDavUrl": format!("https://{host}/personal/ada/Documents/Microsoft%20Teams%20Chat%20Files/q3.pdf")});
        if let Some((quick, sha)) = hashes {
            item["file"] = json!({"hashes": {"quickXorHash": quick, "sha256Hash": sha}});
        }
        item
    }

    /// An item that reports no hash at all.
    fn item(size: u64) -> Value {
        item_json(size, "item-1", "contoso-my.sharepoint.com", None)
    }

    /// An item whose reported hashes are those of `content`.
    fn hashed_item(content: &[u8]) -> Value {
        let hashes = ContentHashes::of_bytes(content);
        item_json(
            content.len() as u64,
            "item-1",
            "contoso-my.sharepoint.com",
            Some((&hashes.quick_xor, &hashes.sha256)),
        )
    }

    fn transport(state: &Arc<State>, cloud: GraphCloud) -> Microsoft365Transport {
        let mut transport = Microsoft365Transport::with_client_in(
            cloud,
            "sensitive-access-token",
            Box::new(Client(Arc::clone(state))),
        );
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        transport
    }

    fn chat() -> ProviderResourceRef {
        ProviderResourceRef::microsoft365("19:chat@thread.v2")
    }

    struct Picked {
        source: LocalUploadSource,
        dir: PathBuf,
    }

    impl Drop for Picked {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn pick(name: &str, content: &[u8]) -> Picked {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teamspace-upload-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        Picked {
            source: LocalUploadSource::from_pick(path).expect("a valid pick"),
            dir,
        }
    }

    fn upload(
        transport: &mut Microsoft365Transport,
        source: &LocalUploadSource,
    ) -> Result<CommandReceipt, TransportError> {
        transport.dispatch(WorkspaceCommand::UploadChatFile {
            chat: chat(),
            source: source.clone(),
        })
    }

    fn script_success(state: &State, size: u64) {
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state
            .puts
            .lock()
            .unwrap()
            .push_back(resp(201, json!({"id": "item-1"})));
        state.gets.lock().unwrap().push_back(resp(200, item(size)));
    }

    #[test]
    fn a_free_name_is_checked_then_put_with_fail_then_read_back() {
        let state = Arc::new(State::default());
        script_success(&state, 5);
        let picked = pick("q3.pdf", b"hello");
        let mut t = transport(&state, GraphCloud::Global);
        let receipt = upload(&mut t, &picked.source).unwrap();
        let log = state.log.lock().unwrap().clone();
        assert_eq!(log.len(), 3, "{log:?}");
        assert!(log[0].starts_with("GET https://graph.microsoft.com/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/q3.pdf?"));
        assert_eq!(
            log[1],
            "PUT https://graph.microsoft.com/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/q3.pdf:/content?@microsoft.graph.conflictBehavior=fail 5 application/octet-stream"
        );
        assert!(log[2].contains("%24select") || log[2].contains("$select"));
        assert_eq!(state.put_bodies.lock().unwrap()[0], b"hello");
        let Some(UploadReceipt::Done(handle)) = receipt.upload else {
            panic!("a finished file");
        };
        assert_eq!(handle.reference_id(), GUID);
        assert!(!handle.is_recovered());
        assert_eq!(handle.name(), "q3.pdf");
        assert_eq!(receipt.disposition, CommandDisposition::AppliedRemotely);
    }

    #[test]
    fn every_cloud_uses_its_own_graph_host() {
        for cloud in GraphCloud::ALL {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .puts
                .lock()
                .unwrap()
                .push_back(resp(201, json!({"id": "item-1"})));
            let host = format!("contoso-my.{}", cloud.content_host_suffixes()[0]);
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, item_json(5, "item-1", &host, None)));
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, cloud);
            upload(&mut t, &picked.source).unwrap();
            for line in state.log.lock().unwrap().iter() {
                let url = line.split(' ').nth(1).unwrap();
                assert_eq!(
                    Url::parse(url).unwrap().host_str(),
                    Some(cloud.graph_host())
                );
            }
        }
    }

    #[test]
    fn the_name_is_percent_encoded_in_the_route() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state
            .puts
            .lock()
            .unwrap()
            .push_back(resp(201, json!({"id": "item-1"})));
        state.gets.lock().unwrap().push_back(resp(200, item(3)));
        let picked = pick("naïve & co (1).txt", b"abc");
        let mut t = transport(&state, GraphCloud::Global);
        upload(&mut t, &picked.source).unwrap();
        let put = state.log.lock().unwrap()[1].clone();
        assert!(
            put.contains("na%C3%AFve%20&%20co%20(1).txt:/content"),
            "{put}"
        );
    }

    #[test]
    fn an_existing_name_is_a_conflict_and_nothing_is_put() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(200, item(5)));
        let picked = pick("q3.pdf", b"hello");
        let mut t = transport(&state, GraphCloud::Global);
        assert_eq!(
            upload(&mut t, &picked.source).unwrap_err(),
            TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into())
        );
        assert!(state.puts.lock().unwrap().is_empty());
        assert_eq!(state.log.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_409_on_the_put_is_a_conflict() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state.puts.lock().unwrap().push_back(resp(409, json!({})));
        let picked = pick("q3.pdf", b"hello");
        let mut t = transport(&state, GraphCloud::Global);
        assert_eq!(
            upload(&mut t, &picked.source).unwrap_err(),
            TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into())
        );
        assert_eq!(
            state.log.lock().unwrap().len(),
            2,
            "no read-back after a 409"
        );
    }

    #[test]
    fn put_statuses_map_without_a_replay() {
        for (status, check) in [(507, 0), (413, 0), (403, 1), (429, 2), (400, 0), (401, 3)] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .puts
                .lock()
                .unwrap()
                .push_back(resp(status, json!({})));
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, GraphCloud::Global);
            let error = upload(&mut t, &picked.source).unwrap_err();
            match check {
                0 => assert!(
                    matches!(error, TransportError::InvalidCommand(_)),
                    "{status}"
                ),
                1 => assert_eq!(error, TransportError::Unauthorized),
                2 => assert!(matches!(error, TransportError::Throttled { .. })),
                _ => assert_eq!(error, TransportError::ReauthenticationRequired),
            }
            assert!(state.puts.lock().unwrap().is_empty(), "{status}");
            assert_eq!(
                state.put_bodies.lock().unwrap().len(),
                1,
                "one PUT only ({status})"
            );
        }
    }

    #[test]
    fn an_ambiguous_put_does_one_get_and_zero_reuploads_and_offers_a_recovered_file() {
        for outcome in [Err(GraphHttpError::Network), resp(503, json!({}))] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state.puts.lock().unwrap().push_back(outcome);
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, hashed_item(b"hello")));
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, GraphCloud::Global);
            let receipt = upload(&mut t, &picked.source).unwrap();
            let Some(UploadReceipt::Done(handle)) = receipt.upload else {
                panic!("a recovered file");
            };
            assert!(handle.is_recovered());
            assert_eq!(
                state.put_bodies.lock().unwrap().len(),
                1,
                "never re-uploaded"
            );
            assert_eq!(
                state
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|line| line.starts_with("GET"))
                    .count(),
                2,
                "the pre-check and exactly one look"
            );
        }
    }

    #[test]
    fn an_ambiguous_put_with_no_file_or_a_different_size_is_uncertain() {
        for follow_up in [resp(404, json!({})), resp(200, item(999))] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .puts
                .lock()
                .unwrap()
                .push_back(Err(GraphHttpError::Network));
            state.gets.lock().unwrap().push_back(follow_up);
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, GraphCloud::Global);
            assert!(matches!(
                upload(&mut t, &picked.source),
                Err(TransportError::Unavailable(_))
            ));
            assert_eq!(state.put_bodies.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn a_file_changed_since_it_was_picked_is_refused_before_any_request() {
        let state = Arc::new(State::default());
        let picked = pick("q3.pdf", b"hello");
        std::fs::write(picked.source.path(), b"hello, longer now").unwrap();
        let mut t = transport(&state, GraphCloud::Global);
        assert!(matches!(
            upload(&mut t, &picked.source),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.log.lock().unwrap().is_empty());
        let gone = pick("gone.txt", b"x");
        std::fs::remove_file(gone.source.path()).unwrap();
        assert!(matches!(
            upload(&mut t, &gone.source),
            Err(TransportError::InvalidCommand(_))
        ));
    }

    #[test]
    fn a_read_back_without_a_usable_etag_or_url_cannot_be_attached() {
        for broken in [
            json!({"size": 5, "webDavUrl": "https://x.sharepoint.com/a"}),
            json!({"size": 5, "eTag": "\"{not-a-guid},1\"", "webDavUrl": "https://x.sharepoint.com/a"}),
            json!({"size": 5, "eTag": format!("\"{{{GUID}}},1\"")}),
            json!({"size": 5, "eTag": format!("\"{{{GUID}}},1\""), "webDavUrl": "http://x.sharepoint.com/a"}),
            json!({"size": 5, "eTag": format!("\"{{{GUID}}},1\""), "webDavUrl": "https://u:p@x.sharepoint.com/a"}),
        ] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state.puts.lock().unwrap().push_back(resp(201, json!({})));
            state.gets.lock().unwrap().push_back(resp(200, broken));
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, GraphCloud::Global);
            assert!(matches!(
                upload(&mut t, &picked.source),
                Err(TransportError::Unavailable(_))
            ));
        }
    }

    #[test]
    fn the_registry_resolves_only_a_matching_live_handle_and_is_bounded() {
        let mut registry = UploadRegistry::default();
        let id = registry.register(
            "a.txt".into(),
            GUID.into(),
            "https://x.sharepoint.com/a".into(),
        );
        let handle = UploadedFileHandle::new(id, "a.txt".into(), 1, GUID.into(), false);
        assert!(registry.resolve(&handle).is_some());
        let wrong_reference = UploadedFileHandle::new(id, "a.txt".into(), 1, "other".into(), false);
        assert!(registry.resolve(&wrong_reference).is_none());
        for index in 0..20 {
            registry.register(format!("{index}.txt"), GUID.into(), "https://x/".into());
        }
        assert!(registry.resolve(&handle).is_none(), "oldest dropped");
        registry.clear();
        assert_eq!(registry.entries.len(), 0);
        let next = registry.register("b".into(), GUID.into(), "https://x/".into());
        assert!(next > id, "ids never restart");
        assert!(!format!("{registry:?}").contains("sharepoint"));
    }

    #[test]
    fn etag_guid_accepts_only_a_real_guid() {
        assert_eq!(
            etag_guid(&format!("\"{{{}}},3\"", GUID.to_ascii_uppercase())),
            Some(GUID.into())
        );
        assert_eq!(etag_guid("\"abc,3\""), None);
        assert_eq!(etag_guid("\"{12345},3\""), None);
        assert_eq!(etag_guid(""), None);
    }

    #[test]
    fn a_local_chat_target_is_refused_before_the_adapter_runs() {
        let picked = pick("q3.pdf", b"hello");
        let state = Arc::new(State::default());
        let mut t = transport(&state, GraphCloud::Global);
        let result = t.dispatch(WorkspaceCommand::UploadChatFile {
            chat: ProviderResourceRef::local_preview("local-chat"),
            source: picked.source.clone(),
        });
        assert!(matches!(result, Err(TransportError::Unsupported(_))));
        assert!(state.log.lock().unwrap().is_empty());
    }

    /// Runs one upload of "hello" whose PUT answered `put` and whose read-back
    /// was `read_back`, and returns the result.
    fn upload_with(
        put: Result<GraphHttpResponse, GraphHttpError>,
        read_back: Value,
    ) -> Result<CommandReceipt, TransportError> {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state.puts.lock().unwrap().push_back(put);
        state.gets.lock().unwrap().push_back(resp(200, read_back));
        let picked = pick("q3.pdf", b"hello");
        let mut t = transport(&state, GraphCloud::Global);
        upload(&mut t, &picked.source)
    }

    fn handle_of(result: Result<CommandReceipt, TransportError>) -> UploadedFileHandle {
        match result.expect("an upload").upload {
            Some(UploadReceipt::Done(handle)) => handle,
            other => panic!("a finished file, got {other:?}"),
        }
    }

    fn is_uncertain(result: &Result<CommandReceipt, TransportError>) -> bool {
        matches!(result, Err(TransportError::Unavailable(_)))
    }

    #[test]
    fn recovery_adopts_a_file_only_on_an_exact_hash_match() {
        let lost = || Err(GraphHttpError::Network);
        // Matching quickXorHash alone, matching sha256Hash alone, both.
        let local = ContentHashes::of_bytes(b"hello");
        for hashes in [
            json!({"quickXorHash": local.quick_xor}),
            json!({"sha256Hash": local.sha256}),
            json!({"quickXorHash": local.quick_xor, "sha256Hash": local.sha256}),
        ] {
            let mut read_back = item(5);
            read_back["file"] = json!({"hashes": hashes});
            assert!(handle_of(upload_with(lost(), read_back)).is_recovered());
        }
        // Same name, same size, different bytes.
        let other = ContentHashes::of_bytes(b"HELLO");
        for hashes in [
            json!({"quickXorHash": other.quick_xor}),
            json!({"sha256Hash": other.sha256}),
            // One matching hash never outweighs a differing one.
            json!({"quickXorHash": local.quick_xor, "sha256Hash": other.sha256}),
            json!({"quickXorHash": other.quick_xor, "sha256Hash": local.sha256}),
        ] {
            let mut read_back = item(5);
            read_back["file"] = json!({"hashes": hashes});
            assert!(is_uncertain(&upload_with(lost(), read_back)));
        }
    }

    #[test]
    fn recovery_never_adopts_a_same_size_file_that_reports_no_usable_hash() {
        for file in [
            None,
            Some(json!({})),
            Some(json!({"hashes": {}})),
            // sha1 and crc32 are never trusted to vouch for a file.
            Some(
                json!({"hashes": {"sha1Hash": "AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D", "crc32Hash": "3610A686"}}),
            ),
            Some(json!({"hashes": {"quickXorHash": ""}})),
        ] {
            let mut read_back = item(5);
            if let Some(file) = file {
                read_back["file"] = file;
            }
            assert!(is_uncertain(&upload_with(
                Err(GraphHttpError::Network),
                read_back
            )));
        }
    }

    #[test]
    fn a_put_answer_without_an_item_id_is_recovered_only_by_hash() {
        // 201 with no usable id behaves like a lost answer.
        for answer in [
            json!({}),
            json!({"id": ""}),
            json!({"id": 7}),
            json!("nope"),
        ] {
            assert!(
                is_uncertain(&upload_with(resp(201, answer.clone()), item(5))),
                "{answer}"
            );
            assert!(
                handle_of(upload_with(resp(201, answer), hashed_item(b"hello"))).is_recovered()
            );
        }
    }

    #[test]
    fn a_read_back_item_must_be_the_item_the_put_answered_with() {
        // The same id: verified, with or without a reported hash.
        for read_back in [item(5), hashed_item(b"hello")] {
            let handle = handle_of(upload_with(resp(201, json!({"id": "item-1"})), read_back));
            assert!(!handle.is_recovered());
        }
        // Another item at that path (someone swapped it): refused, even when
        // its hashes happen to match.
        let mut swapped = hashed_item(b"hello");
        swapped["id"] = json!("item-2");
        assert!(is_uncertain(&upload_with(
            resp(201, json!({"id": "item-1"})),
            swapped
        )));
        let mut swapped = item(5);
        swapped["id"] = json!("item-2");
        assert!(is_uncertain(&upload_with(
            resp(201, json!({"id": "item-1"})),
            swapped
        )));
    }

    #[test]
    fn the_size_is_always_compared_and_a_differing_hash_always_refuses() {
        // Verified by id, but not the same size.
        assert!(is_uncertain(&upload_with(
            resp(201, json!({"id": "item-1"})),
            item(6)
        )));
        // Verified by id and size, but a reported hash differs.
        let other = ContentHashes::of_bytes(b"HELLO");
        let mut read_back = item(5);
        read_back["file"] = json!({"hashes": {"quickXorHash": other.quick_xor}});
        assert!(is_uncertain(&upload_with(
            resp(201, json!({"id": "item-1"})),
            read_back
        )));
    }

    #[test]
    fn the_content_url_must_be_on_this_clouds_content_hosts() {
        let try_url = |cloud: GraphCloud, url: &str| {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .puts
                .lock()
                .unwrap()
                .push_back(resp(201, json!({"id": "item-1"})));
            let mut read_back = item(5);
            read_back["webDavUrl"] = json!(url);
            state.gets.lock().unwrap().push_back(resp(200, read_back));
            let picked = pick("q3.pdf", b"hello");
            let mut t = transport(&state, cloud);
            upload(&mut t, &picked.source).is_ok()
        };
        assert!(try_url(
            GraphCloud::Global,
            "https://contoso-my.sharepoint.com/personal/ada/f.pdf"
        ));
        for bad in [
            "https://evil.example.com/personal/ada/f.pdf",
            "https://sharepoint.com.evil.test/f.pdf",
            "https://evilsharepoint.com/f.pdf",
            "https://sharepoint.com/f.pdf",
            "https://contoso-my.sharepoint.com:8443/f.pdf",
            "https://127.0.0.1/f.pdf",
            "http://contoso-my.sharepoint.com/f.pdf",
        ] {
            assert!(!try_url(GraphCloud::Global, bad), "{bad}");
        }
        // A commercial host is refused by every national cloud.
        for cloud in [GraphCloud::UsGov, GraphCloud::UsGovDod, GraphCloud::China] {
            assert!(
                !try_url(cloud, "https://contoso-my.sharepoint.com/f.pdf"),
                "{cloud:?}"
            );
        }
    }

    #[test]
    fn a_throttled_put_is_reported_as_throttled_and_never_replayed() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state.puts.lock().unwrap().push_back(Ok(GraphHttpResponse {
            status: 429,
            retry_after_seconds: Some(9),
            body: Vec::new(),
        }));
        let picked = pick("q3.pdf", b"hello");
        let mut t = transport(&state, GraphCloud::Global);
        assert_eq!(
            upload(&mut t, &picked.source).unwrap_err(),
            TransportError::Throttled {
                retry_after_seconds: Some(9)
            }
        );
        assert_eq!(state.put_bodies.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_swapped_or_special_file_is_refused_without_blocking() {
        use std::os::unix::fs::symlink;
        let picked = pick("q3.pdf", b"hello");
        let path = picked.source.path().to_path_buf();

        // The same path now holds a different file (a different inode of the
        // same size and, very likely, the same second): refused.
        let replacement = path.with_extension("new");
        std::fs::write(&replacement, b"world").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        // Restore the picked modified time so only the inode differs.
        if let Some(modified) = picked.source.modified() {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(modified).unwrap();
        }
        assert!(matches!(
            open_unchanged(&picked.source),
            Err(TransportError::InvalidCommand(_))
        ));

        // A symlink in its place is never followed.
        let target = path.with_extension("target");
        std::fs::write(&target, b"hello").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&target, &path).unwrap();
        assert!(matches!(
            open_unchanged(&picked.source),
            Err(TransportError::InvalidCommand(_))
        ));

        // A FIFO in its place neither blocks nor opens.
        std::fs::remove_file(&path).unwrap();
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            open_unchanged(&picked.source),
            Err(TransportError::InvalidCommand(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_pick_through_a_symlink_resolves_it_once_and_still_uploads() {
        use std::os::unix::fs::symlink;
        let picked = pick("q3.pdf", b"hello");
        let link = picked.dir.join("alias.pdf");
        symlink(picked.source.path(), &link).unwrap();
        let source = LocalUploadSource::from_pick(link).expect("a link to a file");
        // The name is the one the user picked; the path no longer is a link.
        assert_eq!(source.name(), "alias.pdf");
        assert_eq!(source.path(), picked.source.path());
        assert!(open_unchanged(&source).is_ok());
    }

    #[test]
    fn a_changed_modified_time_is_refused() {
        let picked = pick("q3.pdf", b"hello");
        let file = std::fs::File::options()
            .write(true)
            .open(picked.source.path())
            .unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        assert!(matches!(
            open_unchanged(&picked.source),
            Err(TransportError::InvalidCommand(_))
        ));
    }
}
