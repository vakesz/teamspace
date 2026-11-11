//! Chat file upload by upload session, for files over 4 MiB (up to 100 MiB).
//!
//! `UploadChatFile` for a large file opens the session and sends nothing:
//!
//! 1. one `GET` of the exact path must answer `404` (a `200` is a name
//!    conflict);
//! 2. one `POST .../root:/Microsoft Teams Chat Files/{name}:/createUploadSession`
//!    with `{"item": {"@microsoft.graph.conflictBehavior": "fail"}}`; its
//!    `uploadUrl` is a secret that carries its own credential, so it must be
//!    HTTPS on a subdomain of the configured cloud's content hosts
//!    (`GraphCloud::content_host_suffixes`, through the same check as the
//!    preview hop) with no credentials, port or fragment, is kept only here
//!    (zeroized on drop, never logged, never in `Debug`) and is never sent
//!    with the Graph bearer token.
//!
//! Each fragment is then its own `ContinueChatUpload` job (3.75 MiB, a multiple
//! of 320 KiB, in order) so a cancellation, another command or a refresh can run
//! between them. A fragment `PUT` to the session URL is idempotent by protocol,
//! so after a network error, `5xx`, `429` or `416` the adapter asks the session
//! for its `nextExpectedRanges` once and resumes from there, at most three times
//! without progress. The picked file must still have its size and modified time
//! at every fragment, or the session is cancelled. The last fragment answers
//! `200`/`201`; one `GET` of the exact path then reads the `eTag` GUID and
//! `webDavUrl` for the handle, exactly as for a small file. A `409` on the last
//! fragment is a name conflict. `CancelChatUpload` sends `DELETE` to the session
//! URL. See Graph's [upload session
//! documentation](https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession).

use super::chat_uploads::{
    CHANGED_MESSAGE, CHAT_FILES_FOLDER, ITEM_SELECT, ReadBack, UNAVAILABLE_MESSAGE,
    UNCERTAIN_MESSAGE, answered_item_id, get_item, item_url, judge_read_back, name_is_taken,
    open_picked, open_unchanged,
};
use super::chats::selected_chat_identifier;
use super::content_hash::ContentHashes;
use super::file_preview::preauthorized_content_url;
use super::{GraphHttpError, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::transport::{
    CommandDisposition, LocalUploadSource, MAX_CHAT_UPLOAD_SESSION_BYTES, UPLOAD_FRAGMENT_BYTES,
    UPLOAD_NAME_CONFLICT, UploadProgress, UploadReceipt, UploadSessionRef, UploadedFileHandle,
    validate_upload_name,
};
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::SystemTime;
use url::Url;
use zeroize::Zeroizing;

const MAX_SESSION_ANSWER_BYTES: u64 = 16 * 1024;
const MAX_OPEN_SESSIONS: usize = 2;
/// Consecutive fragments without progress before a session is abandoned.
const MAX_STALLS: u8 = 3;

const SESSION_GONE_MESSAGE: &str = "The upload is no longer available. Pick the file again.";
const SESSION_REJECTED_MESSAGE: &str = "OneDrive rejected the file upload";

struct OpenSession {
    id: u64,
    /// The session URL is the credential for its fragments.
    upload_url: Zeroizing<String>,
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    identity: Option<(u64, u64)>,
    name: String,
    /// The next byte to send.
    offset: u64,
    stalls: u8,
}

/// The adapter's open upload sessions. Bounded, cleared on
/// reauthentication, and never logged: `Debug` shows a count.
#[derive(Default)]
pub(super) struct UploadSessions {
    next_id: u64,
    entries: Vec<OpenSession>,
}

impl std::fmt::Debug for UploadSessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadSessions")
            .field("open", &self.entries.len())
            .finish()
    }
}

impl UploadSessions {
    /// Session IDs only ever grow, from a random start per adapter, so a stale
    /// reference never names a different session.
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

    fn position(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Microsoft365Transport {
    /// Opens the session for a file over 4 MiB. Nothing of the file is sent.
    pub(super) fn start_upload_session(
        &mut self,
        chat: &ProviderResourceRef,
        source: &LocalUploadSource,
    ) -> Result<CommandReceipt, TransportError> {
        selected_chat_identifier(chat)?;
        validate_upload_name(source.name())
            .map_err(|error| TransportError::InvalidCommand(error.user_message().into()))?;
        if source.size() > MAX_CHAT_UPLOAD_SESSION_BYTES {
            return Err(TransportError::InvalidCommand(
                "Files over 100 MiB cannot be attached.".into(),
            ));
        }
        // The file must still be the one that was picked.
        open_unchanged(source)?;

        // Step 1: the name must be free.
        let item = item_url(self.session.cloud(), source.name(), Some(ITEM_SELECT))?;
        match name_is_taken(&self.session, &item) {
            Ok(true) => {
                return Err(TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into()));
            }
            Ok(false) => {}
            Err(failure) => return Err(self.upload_failure(failure)),
        }

        // Step 2: the session. A write, sent once.
        let create_url = self
            .session
            .cloud()
            .graph_route(
                &[
                    "me",
                    "drive",
                    "root:",
                    CHAT_FILES_FOLDER,
                    &format!("{}:", source.name()),
                    "createUploadSession",
                ],
                None,
            )
            .map_err(|_| {
                TransportError::Unavailable(
                    "could not prepare the Microsoft Graph upload route".into(),
                )
            })?;
        let body = serde_json::to_vec(&json!({
            "item": {
                "@microsoft.graph.conflictBehavior": "fail",
                "name": source.name(),
            }
        }))
        .map_err(|_| TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()))?;
        let response = self
            .session
            .post_json_once(
                &GraphJsonRequest {
                    url: &create_url,
                    content_type: "application/json",
                    body: &body,
                },
                MAX_SESSION_ANSWER_BYTES,
            )
            .map_err(map_session_http_error)?;
        match response.status {
            200 => {}
            409 => return Err(TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into())),
            401 => {
                return Err(self
                    .upload_failure(super::chat_uploads::UploadFailure::ReauthenticationRequired));
            }
            403 => return Err(TransportError::Unauthorized),
            429 => {
                return Err(TransportError::Throttled {
                    retry_after_seconds: response.retry_after_seconds,
                });
            }
            400 | 404 | 413 | 422 | 507 => {
                return Err(TransportError::InvalidCommand(
                    SESSION_REJECTED_MESSAGE.into(),
                ));
            }
            _ => return Err(TransportError::Unavailable(UNAVAILABLE_MESSAGE.into())),
        }
        let upload_url = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|value| {
                value
                    .get("uploadUrl")
                    .and_then(Value::as_str)
                    .map(|url| Zeroizing::new(url.to_owned()))
            })
            .and_then(|url| validated_upload_url(self.session.cloud(), &url))
            .ok_or_else(|| TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()))?;

        // Bounded: the oldest open session is cancelled to make room.
        while self.upload_sessions.entries.len() >= MAX_OPEN_SESSIONS {
            self.abort_session(0);
        }
        let id = self.upload_sessions.allocate_id();
        self.upload_sessions.entries.push(OpenSession {
            id,
            upload_url,
            path: source.path().to_path_buf(),
            size: source.size(),
            modified: source.modified(),
            identity: source.identity(),
            name: source.name().to_owned(),
            offset: 0,
            stalls: 0,
        });
        Ok(progress_receipt(
            "microsoft365:chat-upload-session",
            id,
            0,
            source.size(),
        ))
    }

    /// Uploads one fragment of an open session.
    pub(super) fn continue_chat_upload(
        &mut self,
        chat: &ProviderResourceRef,
        session: UploadSessionRef,
    ) -> Result<CommandReceipt, TransportError> {
        selected_chat_identifier(chat)?;
        let Some(index) = self.upload_sessions.position(session.id()) else {
            return Err(TransportError::InvalidCommand(SESSION_GONE_MESSAGE.into()));
        };
        let (path, size, modified, identity, offset) = {
            let open = &self.upload_sessions.entries[index];
            (
                open.path.clone(),
                open.size,
                open.modified,
                open.identity,
                open.offset,
            )
        };
        // The file must still be the one that was picked, at every fragment.
        let mut file = match open_picked(&path, size, modified, identity) {
            Ok(file) => file,
            Err(error) => {
                self.abort_session(index);
                return Err(error);
            }
        };
        let end = (offset + UPLOAD_FRAGMENT_BYTES).min(size);
        let length = usize::try_from(end - offset).unwrap_or(0);
        let mut bytes = Vec::with_capacity(length);
        let read = file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| file.take(end - offset).read_to_end(&mut bytes));
        if read.is_err() || bytes.len() != length || length == 0 {
            self.abort_session(index);
            return Err(TransportError::InvalidCommand(CHANGED_MESSAGE.into()));
        }
        let range = format!("bytes {offset}-{}/{size}", end - 1);
        let Some(url) = self.session_url(index) else {
            self.abort_session(index);
            return Err(TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()));
        };
        let answer =
            self.session
                .put_session_fragment(&url, &range, &bytes, MAX_SESSION_ANSWER_BYTES);
        drop(bytes);
        match answer {
            Ok(response) => match response.status {
                200 | 201 => {
                    let answered = answered_item_id(&response.body);
                    self.finish_session(index, answered)
                }
                202 => {
                    let next = next_offset(&response.body, end, size);
                    self.record_progress(index, offset, next)
                }
                409 => {
                    self.abort_session(index);
                    Err(TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into()))
                }
                404 | 410 => {
                    self.upload_sessions.entries.remove(index);
                    Err(TransportError::InvalidCommand(SESSION_GONE_MESSAGE.into()))
                }
                400 | 401 | 403 | 413 | 422 | 507 => {
                    self.abort_session(index);
                    Err(TransportError::InvalidCommand(
                        SESSION_REJECTED_MESSAGE.into(),
                    ))
                }
                // 416, 429, 5xx and the rest leave the fragment's outcome
                // unknown: ask the session where it stands.
                _ => self.resume_from_status(index, offset),
            },
            Err(_) => self.resume_from_status(index, offset),
        }
    }

    /// Cancels an open session and tells OneDrive to discard what it holds.
    /// Best effort: the session is forgotten either way.
    pub(super) fn cancel_chat_upload(
        &mut self,
        chat: &ProviderResourceRef,
        session: UploadSessionRef,
    ) -> Result<CommandReceipt, TransportError> {
        selected_chat_identifier(chat)?;
        if let Some(index) = self.upload_sessions.position(session.id()) {
            self.abort_session(index);
        }
        Ok(CommandReceipt {
            command_id: "microsoft365:chat-upload-cancel".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        })
    }

    fn session_url(&self, index: usize) -> Option<Url> {
        Url::parse(&self.upload_sessions.entries[index].upload_url).ok()
    }

    /// Forgets a session and sends one best-effort `DELETE`.
    fn abort_session(&mut self, index: usize) {
        let open = self.upload_sessions.entries.remove(index);
        if let Ok(url) = Url::parse(&open.upload_url) {
            let _ = self.session.delete_session(&url);
        }
    }

    /// Moves the session to the offset the server asked for. No progress,
    /// three times running, abandons the session.
    fn record_progress(
        &mut self,
        index: usize,
        sent_from: u64,
        next: Option<u64>,
    ) -> Result<CommandReceipt, TransportError> {
        let open = &mut self.upload_sessions.entries[index];
        let total = open.size;
        let Some(next) = next.filter(|next| *next < total) else {
            self.abort_session(index);
            return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
        };
        if next <= sent_from {
            open.stalls += 1;
        } else {
            open.stalls = 0;
        }
        if open.stalls > MAX_STALLS {
            self.abort_session(index);
            return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
        }
        open.offset = next;
        let id = open.id;
        Ok(progress_receipt(
            "microsoft365:chat-upload-fragment",
            id,
            next,
            total,
        ))
    }

    /// After an unknown fragment outcome: one status read, then resume from
    /// what the server holds. Never more than three tries without progress.
    fn resume_from_status(
        &mut self,
        index: usize,
        sent_from: u64,
    ) -> Result<CommandReceipt, TransportError> {
        let Some(url) = self.session_url(index) else {
            self.abort_session(index);
            return Err(TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()));
        };
        let status = self
            .session
            .get_session_status(&url, MAX_SESSION_ANSWER_BYTES);
        match status {
            Ok(response) if response.status == 200 => {
                let size = self.upload_sessions.entries[index].size;
                let next = next_offset(&response.body, sent_from, size);
                self.record_progress(index, sent_from, next)
            }
            Ok(response) if matches!(response.status, 404 | 410) => {
                self.upload_sessions.entries.remove(index);
                Err(TransportError::InvalidCommand(SESSION_GONE_MESSAGE.into()))
            }
            // The session cannot be asked: the feature is told the upload is
            // uncertain and cancels the session.
            _ => Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into())),
        }
    }

    /// The last fragment is in: read the file back by its exact path for its
    /// `eTag` GUID and `webDavUrl`, as for a small file.
    fn finish_session(
        &mut self,
        index: usize,
        answered_id: Option<String>,
    ) -> Result<CommandReceipt, TransportError> {
        let open = self.upload_sessions.entries.remove(index);
        let item = item_url(self.session.cloud(), &open.name, Some(ITEM_SELECT))?;
        let found = match get_item(&self.session, &item) {
            Ok(found) => found,
            Err(super::chat_uploads::UploadFailure::ReauthenticationRequired) => {
                return Err(self
                    .upload_failure(super::chat_uploads::UploadFailure::ReauthenticationRequired));
            }
            Err(_) => None,
        };
        let Some(info) = found else {
            return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
        };
        // The bytes that were sent are hashed again from the picked file (still
        // the same file, or this fails) and compared with what OneDrive holds.
        let local = open_picked(&open.path, open.size, open.modified, open.identity)
            .ok()
            .and_then(|file| ContentHashes::of_reader(file).ok());
        let recovered = match local
            .as_ref()
            .map(|local| judge_read_back(&info, answered_id.as_deref(), open.size, local))
        {
            Some(ReadBack::Verified) => false,
            Some(ReadBack::Recovered) => true,
            Some(ReadBack::Unproven) | None => {
                return Err(TransportError::Unavailable(UNCERTAIN_MESSAGE.into()));
            }
        };
        let id = self.uploads.register_item(
            open.name.clone(),
            info.reference.clone(),
            info.content_url,
            info.id,
        );
        Ok(CommandReceipt {
            command_id: "microsoft365:chat-upload".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(UploadReceipt::Done(UploadedFileHandle::new(
                id,
                open.name,
                open.size,
                info.reference,
                recovered,
            ))),
        })
    }
}

fn progress_receipt(command_id: &'static str, id: u64, sent: u64, total: u64) -> CommandReceipt {
    CommandReceipt {
        command_id: command_id.into(),
        disposition: CommandDisposition::AppliedRemotely,
        upload: Some(UploadReceipt::Progress(UploadProgress::new(
            UploadSessionRef::new(id),
            sent,
            total,
        ))),
    }
}

fn map_session_http_error(error: GraphHttpError) -> TransportError {
    match error {
        GraphHttpError::Network | GraphHttpError::ResponseTooLarge => {
            TransportError::Unavailable(UNAVAILABLE_MESSAGE.into())
        }
        GraphHttpError::Unsupported | GraphHttpError::RenewalUnavailable => {
            TransportError::Unavailable(UNAVAILABLE_MESSAGE.into())
        }
        GraphHttpError::RenewedBeforeDelivery => TransportError::InvalidCommand(
            "Microsoft 365 renewed the session before the upload started; pick the file again"
                .into(),
        ),
    }
}

/// A session URL is accepted only on a content host of this cloud, with no
/// credential, port or fragment (see the module docs).
pub(super) fn validated_upload_url(
    cloud: crate::integrations::cloud::GraphCloud,
    value: &str,
) -> Option<Zeroizing<String>> {
    let url = preauthorized_content_url(cloud, value)?;
    url.port()
        .is_none()
        .then(|| Zeroizing::new(url.to_string()))
}

/// The byte the server wants next, from `nextExpectedRanges`. `end` is where
/// this fragment ended: an empty or unreadable list means "carry on from
/// there". A start beyond `end` (the server cannot be ahead of what was sent)
/// or beyond the file is refused.
fn next_offset(body: &[u8], end: u64, total: u64) -> Option<u64> {
    let ranges = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("nextExpectedRanges").cloned());
    let first = ranges
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|ranges| ranges.first())
        .and_then(Value::as_str);
    let Some(first) = first else {
        return Some(end).filter(|end| *end <= total);
    };
    let start = first.split('-').next()?.trim().parse::<u64>().ok()?;
    (start <= end && start <= total).then_some(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphHttpResponse, GraphHttpStatus};
    use crate::integrations::transport::MAX_CHAT_UPLOAD_BYTES;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, WorkspaceCommand, WorkspaceTransport,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";
    const SESSION_URL: &str = "https://contoso-my.sharepoint.com/personal/ada/_api/v2.0/drives/b!x/uploadSession?guid=g&tempauth=SECRETTEMPAUTH";
    const FRAGMENT: u64 = UPLOAD_FRAGMENT_BYTES;

    #[derive(Default)]
    struct State {
        gets: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        posts: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        fragments: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        statuses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        deletes: Mutex<VecDeque<Result<GraphHttpStatus, GraphHttpError>>>,
        /// Every request: kind, url, extra.
        log: Mutex<Vec<(String, String, String)>>,
        post_bodies: Mutex<Vec<String>>,
        fragment_lengths: Mutex<Vec<usize>>,
        /// A file to grow while the next fragment is on the wire.
        grow_on_fragment: Mutex<Option<PathBuf>>,
    }

    struct Client(Arc<State>);

    impl GraphHttpClient for Client {
        fn get_me(&self, _: &Url, _: &str) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            token: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .log
                .lock()
                .unwrap()
                .push(("GET".into(), url.to_string(), token.into()));
            self.0
                .gets
                .lock()
                .unwrap()
                .pop_front()
                .expect("get scripted")
        }

        fn post_json_query(
            &self,
            request: crate::integrations::microsoft365::GraphJsonRequest<'_>,
            token: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .log
                .lock()
                .unwrap()
                .push(("POST".into(), request.url.to_string(), token.into()));
            self.0
                .post_bodies
                .lock()
                .unwrap()
                .push(String::from_utf8(request.body.to_vec()).unwrap());
            self.0
                .posts
                .lock()
                .unwrap()
                .pop_front()
                .expect("post scripted")
        }

        fn put_session_fragment(
            &self,
            url: &Url,
            content_range: &str,
            body: &[u8],
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0.log.lock().unwrap().push((
                "FRAGMENT".into(),
                url.to_string(),
                content_range.into(),
            ));
            self.0.fragment_lengths.lock().unwrap().push(body.len());
            if let Some(path) = self.0.grow_on_fragment.lock().unwrap().take() {
                let file = std::fs::File::options().write(true).open(path).unwrap();
                file.set_len(file.metadata().unwrap().len() + 1).unwrap();
            }
            self.0
                .fragments
                .lock()
                .unwrap()
                .pop_front()
                .expect("fragment scripted")
        }

        fn get_session_status(
            &self,
            url: &Url,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .log
                .lock()
                .unwrap()
                .push(("STATUS".into(), url.to_string(), String::new()));
            self.0
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .expect("status scripted")
        }

        fn delete_session(&self, url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
            self.0
                .log
                .lock()
                .unwrap()
                .push(("DELETE".into(), url.to_string(), String::new()));
            self.0
                .deletes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(GraphHttpStatus {
                    status: 204,
                    retry_after_seconds: None,
                }))
        }
    }

    fn resp(status: u16, body: Value) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.to_string().into_bytes(),
        })
    }

    fn accepted(next: u64) -> Result<GraphHttpResponse, GraphHttpError> {
        resp(202, json!({"nextExpectedRanges": [format!("{next}-")]}))
    }

    fn item(size: u64) -> Value {
        json!({"id": "item-1", "name": "big.bin", "size": size,
               "eTag": format!("\"{{{}}},2\"", GUID.to_ascii_uppercase()),
               "webDavUrl": "https://contoso-my.sharepoint.com/personal/ada/Documents/Microsoft%20Teams%20Chat%20Files/big.bin"})
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

    /// A sparse file of `size` bytes (a few distinct bytes at the edges).
    fn pick(size: u64) -> Picked {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teamspace-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(size).unwrap();
        Picked {
            source: LocalUploadSource::from_pick(path).expect("a valid pick"),
            dir,
        }
    }

    fn start(
        t: &mut Microsoft365Transport,
        source: &LocalUploadSource,
    ) -> Result<CommandReceipt, TransportError> {
        t.dispatch(WorkspaceCommand::UploadChatFile {
            chat: chat(),
            source: source.clone(),
        })
    }

    fn proceed(
        t: &mut Microsoft365Transport,
        session: UploadSessionRef,
    ) -> Result<CommandReceipt, TransportError> {
        t.dispatch(WorkspaceCommand::ContinueChatUpload {
            chat: chat(),
            session,
        })
    }

    fn progress(receipt: CommandReceipt) -> UploadProgress {
        match receipt.upload {
            Some(UploadReceipt::Progress(progress)) => progress,
            other => panic!("progress expected, got {other:?}"),
        }
    }

    fn script_session(state: &State) {
        state.gets.lock().unwrap().push_back(resp(404, json!({})));
        state.posts.lock().unwrap().push_back(resp(
            200,
            json!({"uploadUrl": SESSION_URL, "expirationDateTime": "2026-12-01T00:00:00Z"}),
        ));
    }

    fn kinds(state: &State) -> Vec<String> {
        state
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, ..)| kind.clone())
            .collect()
    }

    #[test]
    fn fragment_math_is_a_multiple_of_320_kib_and_far_below_60_mib() {
        assert_eq!(FRAGMENT % (320 * 1024), 0);
        assert_eq!(FRAGMENT, 3_932_160);
        const { assert!(FRAGMENT < 60 * 1024 * 1024) };
        // 100 MiB is 26 full fragments and one short one of 2.5 MiB.
        let total = MAX_CHAT_UPLOAD_SESSION_BYTES;
        assert_eq!((total / FRAGMENT, total % FRAGMENT), (26, 2_621_440));
    }

    #[test]
    fn a_large_file_opens_a_session_with_fail_and_sends_nothing_yet() {
        let state = Arc::new(State::default());
        script_session(&state);
        let picked = pick(FRAGMENT * 2 + 5);
        let mut t = transport(&state, GraphCloud::Global);
        let receipt = start(&mut t, &picked.source).unwrap();
        let progress = progress(receipt);
        assert_eq!((progress.sent(), progress.total()), (0, FRAGMENT * 2 + 5));
        assert_eq!(kinds(&state), ["GET", "POST"]);
        let log = state.log.lock().unwrap().clone();
        assert!(log[1].1.ends_with(
            "/v1.0/me/drive/root:/Microsoft%20Teams%20Chat%20Files/big.bin:/createUploadSession"
        ));
        let body: Value = serde_json::from_str(&state.post_bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(
            body,
            json!({"item": {"@microsoft.graph.conflictBehavior": "fail", "name": "big.bin"}})
        );
        assert_eq!(t.upload_sessions.len(), 1);
    }

    #[test]
    fn the_whole_upload_runs_fragment_by_fragment_then_reads_the_file_back() {
        let state = Arc::new(State::default());
        script_session(&state);
        let size = FRAGMENT * 2 + 5;
        {
            let mut fragments = state.fragments.lock().unwrap();
            fragments.push_back(accepted(FRAGMENT));
            fragments.push_back(accepted(FRAGMENT * 2));
            fragments.push_back(resp(201, json!({"id": "item-1"})));
        }
        state.gets.lock().unwrap().push_back(resp(200, item(size)));
        let picked = pick(size);
        let mut t = transport(&state, GraphCloud::Global);
        let mut next = progress(start(&mut t, &picked.source).unwrap());
        let mut done = None;
        for round in 0..3 {
            let receipt = proceed(&mut t, next.session()).unwrap();
            match receipt.upload {
                Some(UploadReceipt::Progress(progress)) => {
                    assert!(round < 2);
                    next = progress;
                }
                Some(UploadReceipt::Done(handle)) => done = Some(handle),
                None => panic!("an upload receipt"),
                Some(UploadReceipt::Shared(_)) => panic!("not a share"),
            }
        }
        let handle = done.expect("the file is done");
        assert_eq!(
            (handle.reference_id(), handle.name(), handle.size()),
            (GUID, "big.bin", size)
        );
        assert_eq!(
            state.fragment_lengths.lock().unwrap().clone(),
            [FRAGMENT as usize, FRAGMENT as usize, 5]
        );
        let ranges: Vec<String> = state
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, ..)| kind == "FRAGMENT")
            .map(|(_, _, range)| range.clone())
            .collect();
        assert_eq!(
            ranges,
            [
                format!("bytes 0-{}/{size}", FRAGMENT - 1),
                format!("bytes {FRAGMENT}-{}/{size}", FRAGMENT * 2 - 1),
                format!("bytes {}-{}/{size}", FRAGMENT * 2, size - 1),
            ]
        );
        assert_eq!(t.upload_sessions.len(), 0);
    }

    #[test]
    fn fragments_never_carry_the_bearer_token_and_only_graph_calls_do() {
        let state = Arc::new(State::default());
        script_session(&state);
        let size = MAX_CHAT_UPLOAD_BYTES + 1;
        state
            .fragments
            .lock()
            .unwrap()
            .push_back(accepted(FRAGMENT));
        state.statuses.lock().unwrap().push_back(resp(
            200,
            json!({"nextExpectedRanges": [format!("{FRAGMENT}-")]}),
        ));
        let picked = pick(size);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        proceed(&mut t, first.session()).unwrap();
        for (kind, url, extra) in state.log.lock().unwrap().iter() {
            match kind.as_str() {
                "GET" | "POST" => {
                    assert_eq!(extra, "sensitive-access-token", "{kind}");
                    assert!(url.starts_with("https://graph.microsoft.com/"), "{url}");
                }
                _ => {
                    assert!(!url.contains("sensitive-access-token"));
                    assert!(!extra.contains("sensitive-access-token"), "{kind}: {extra}");
                    assert_eq!(url, &Url::parse(SESSION_URL).unwrap().to_string());
                }
            }
        }
    }

    #[test]
    fn the_session_url_must_be_https_on_this_clouds_content_hosts() {
        let good = "https://contoso-my.sharepoint.com/personal/x/_api/uploadSession?tempauth=t";
        for cloud in GraphCloud::ALL {
            let host = format!("contoso-my.{}", cloud.content_host_suffixes()[0]);
            let url = format!("https://{host}/personal/x/_api/uploadSession?tempauth=t");
            assert!(validated_upload_url(cloud, &url).is_some(), "{cloud:?}");
        }
        assert!(validated_upload_url(GraphCloud::Global, good).is_some());
        for bad in [
            "http://contoso-my.sharepoint.com/x",
            "https://evil.example.com/x?tempauth=t",
            "https://sharepoint.com.evil.test/x",
            "https://evilsharepoint.com/x",
            "https://sharepoint.com/x",
            "https://user:pw@contoso-my.sharepoint.com/x",
            "https://contoso-my.sharepoint.com:8443/x",
            "https://contoso-my.sharepoint.com/x#frag",
            "https://127.0.0.1/x",
            "https://localhost/x",
            "ftp://contoso-my.sharepoint.com/x",
            "",
        ] {
            assert!(
                validated_upload_url(GraphCloud::Global, bad).is_none(),
                "{bad}"
            );
        }
        // A commercial host is refused by every national cloud, and a national
        // host by the commercial cloud.
        for cloud in [GraphCloud::UsGov, GraphCloud::UsGovDod, GraphCloud::China] {
            assert!(validated_upload_url(cloud, good).is_none(), "{cloud:?}");
        }
        assert!(
            validated_upload_url(GraphCloud::Global, "https://contoso-my.sharepoint.us/x")
                .is_none()
        );
    }

    #[test]
    fn an_offending_session_url_opens_no_session_and_is_never_used() {
        for url in [
            "https://evil.example.com/up?tempauth=t",
            "http://a.sharepoint.com/x",
        ] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .posts
                .lock()
                .unwrap()
                .push_back(resp(200, json!({"uploadUrl": url})));
            let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
            let mut t = transport(&state, GraphCloud::Global);
            assert!(matches!(
                start(&mut t, &picked.source),
                Err(TransportError::Unavailable(_))
            ));
            assert_eq!(t.upload_sessions.len(), 0);
            assert_eq!(
                kinds(&state),
                ["GET", "POST"],
                "nothing was sent to the URL"
            );
        }
    }

    #[test]
    fn the_session_url_is_a_secret_in_every_debug_string() {
        let state = Arc::new(State::default());
        script_session(&state);
        let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
        let mut t = transport(&state, GraphCloud::Global);
        let receipt = start(&mut t, &picked.source).unwrap();
        let debug = format!("{receipt:?} {:?} {:?}", t.upload_sessions, picked.source);
        for secret in ["SECRETTEMPAUTH", "sharepoint", "big.bin", "tempauth"] {
            assert!(!debug.contains(secret), "{secret} leaked");
        }
    }

    #[test]
    fn a_name_that_exists_is_a_conflict_before_any_session_is_created() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(200, item(9)));
        let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
        let mut t = transport(&state, GraphCloud::Global);
        assert_eq!(
            start(&mut t, &picked.source).unwrap_err(),
            TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into())
        );
        assert_eq!(kinds(&state), ["GET"]);
    }

    #[test]
    fn create_session_statuses_map_without_a_replay() {
        for (status, expected) in [
            (409, "conflict"),
            (403, "forbidden"),
            (429, "throttled"),
            (400, "rejected"),
            (507, "rejected"),
            (500, "unavailable"),
            (401, "reauth"),
        ] {
            let state = Arc::new(State::default());
            state.gets.lock().unwrap().push_back(resp(404, json!({})));
            state
                .posts
                .lock()
                .unwrap()
                .push_back(resp(status, json!({})));
            let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
            let mut t = transport(&state, GraphCloud::Global);
            let error = start(&mut t, &picked.source).unwrap_err();
            let ok = match expected {
                "conflict" => error == TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into()),
                "forbidden" => error == TransportError::Unauthorized,
                "throttled" => matches!(error, TransportError::Throttled { .. }),
                "rejected" => matches!(error, TransportError::InvalidCommand(_)),
                "unavailable" => matches!(error, TransportError::Unavailable(_)),
                _ => error == TransportError::ReauthenticationRequired,
            };
            assert!(ok, "{status}: {error:?}");
            assert_eq!(
                state.post_bodies.lock().unwrap().len(),
                1,
                "one POST ({status})"
            );
            assert_eq!(t.upload_sessions.len(), 0);
        }
    }

    #[test]
    fn a_file_changed_between_fragments_cancels_the_session() {
        let state = Arc::new(State::default());
        script_session(&state);
        let picked = pick(FRAGMENT * 2);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        std::fs::File::options()
            .write(true)
            .open(picked.source.path())
            .unwrap()
            .set_len(FRAGMENT * 2 + 1)
            .unwrap();
        assert!(matches!(
            proceed(&mut t, first.session()),
            Err(TransportError::InvalidCommand(_))
        ));
        assert_eq!(
            kinds(&state),
            ["GET", "POST", "DELETE"],
            "no fragment was sent"
        );
        assert_eq!(t.upload_sessions.len(), 0);
    }

    #[test]
    fn a_fragment_for_an_unknown_session_sends_nothing() {
        let state = Arc::new(State::default());
        let mut t = transport(&state, GraphCloud::Global);
        assert!(matches!(
            proceed(&mut t, UploadSessionRef::for_test(77)),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_409_on_the_last_fragment_is_a_name_conflict_and_deletes_the_session() {
        let state = Arc::new(State::default());
        script_session(&state);
        state
            .fragments
            .lock()
            .unwrap()
            .push_back(resp(409, json!({})));
        let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        assert_eq!(
            proceed(&mut t, first.session()).unwrap_err(),
            TransportError::InvalidCommand(UPLOAD_NAME_CONFLICT.into())
        );
        assert_eq!(kinds(&state), ["GET", "POST", "FRAGMENT", "DELETE"]);
    }

    #[test]
    fn an_unknown_fragment_outcome_asks_the_session_once_and_resumes_from_its_answer() {
        for outcome in [
            Err(GraphHttpError::Network),
            resp(503, json!({})),
            resp(416, json!({})),
            resp(429, json!({})),
        ] {
            let state = Arc::new(State::default());
            script_session(&state);
            state.fragments.lock().unwrap().push_back(outcome);
            // The server holds nothing yet: resume from 0.
            state
                .statuses
                .lock()
                .unwrap()
                .push_back(resp(200, json!({"nextExpectedRanges": ["0-"]})));
            let picked = pick(FRAGMENT * 2);
            let mut t = transport(&state, GraphCloud::Global);
            let first = progress(start(&mut t, &picked.source).unwrap());
            let after = progress(proceed(&mut t, first.session()).unwrap());
            assert_eq!(after.sent(), 0);
            assert_eq!(kinds(&state), ["GET", "POST", "FRAGMENT", "STATUS"]);
        }
    }

    #[test]
    fn three_fragments_in_a_row_without_progress_abandon_the_session() {
        let state = Arc::new(State::default());
        script_session(&state);
        let picked = pick(FRAGMENT * 2);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        let mut last = Ok(());
        for _ in 0..(MAX_STALLS as usize + 1) {
            state
                .fragments
                .lock()
                .unwrap()
                .push_back(Err(GraphHttpError::Network));
            state
                .statuses
                .lock()
                .unwrap()
                .push_back(resp(200, json!({"nextExpectedRanges": ["0-"]})));
            last = proceed(&mut t, first.session()).map(|_| ());
        }
        assert!(matches!(last, Err(TransportError::Unavailable(_))));
        assert_eq!(kinds(&state).last().map(String::as_str), Some("DELETE"));
        assert_eq!(t.upload_sessions.len(), 0);
    }

    #[test]
    fn an_unreadable_session_status_is_uncertain_and_keeps_the_session_for_a_cancel() {
        let state = Arc::new(State::default());
        script_session(&state);
        state
            .fragments
            .lock()
            .unwrap()
            .push_back(Err(GraphHttpError::Network));
        state
            .statuses
            .lock()
            .unwrap()
            .push_back(Err(GraphHttpError::Network));
        let picked = pick(FRAGMENT * 2);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        assert!(matches!(
            proceed(&mut t, first.session()),
            Err(TransportError::Unavailable(_))
        ));
        assert_eq!(t.upload_sessions.len(), 1, "still there to cancel");
        t.dispatch(WorkspaceCommand::CancelChatUpload {
            chat: chat(),
            session: first.session(),
        })
        .unwrap();
        assert_eq!(t.upload_sessions.len(), 0);
        assert_eq!(kinds(&state).last().map(String::as_str), Some("DELETE"));
    }

    #[test]
    fn an_expired_session_is_forgotten_and_reported() {
        for status in [404, 410] {
            let state = Arc::new(State::default());
            script_session(&state);
            state
                .fragments
                .lock()
                .unwrap()
                .push_back(resp(status, json!({})));
            let picked = pick(FRAGMENT * 2);
            let mut t = transport(&state, GraphCloud::Global);
            let first = progress(start(&mut t, &picked.source).unwrap());
            assert!(matches!(
                proceed(&mut t, first.session()),
                Err(TransportError::InvalidCommand(_))
            ));
            assert_eq!(t.upload_sessions.len(), 0);
        }
    }

    #[test]
    fn cancel_sends_one_delete_to_the_session_and_is_idempotent() {
        let state = Arc::new(State::default());
        script_session(&state);
        let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        let cancel = |t: &mut Microsoft365Transport| {
            t.dispatch(WorkspaceCommand::CancelChatUpload {
                chat: chat(),
                session: first.session(),
            })
        };
        assert!(cancel(&mut t).is_ok());
        assert!(cancel(&mut t).is_ok(), "a second cancel is harmless");
        assert_eq!(
            kinds(&state)
                .iter()
                .filter(|kind| *kind == "DELETE")
                .count(),
            1
        );
    }

    #[test]
    fn the_bounds_are_100_mib_and_two_open_sessions() {
        // Over the cap: refused by the command before any adapter work.
        let too_big = WorkspaceCommand::UploadChatFile {
            chat: chat(),
            source: LocalUploadSource::for_test(
                PathBuf::from("/tmp/x"),
                MAX_CHAT_UPLOAD_SESSION_BYTES + 1,
                None,
                "x.bin",
            ),
        };
        assert!(too_big.validate().is_err());

        let state = Arc::new(State::default());
        let picked = pick(MAX_CHAT_UPLOAD_BYTES + 1);
        let mut t = transport(&state, GraphCloud::Global);
        for _ in 0..3 {
            script_session(&state);
            start(&mut t, &picked.source).unwrap();
        }
        assert_eq!(t.upload_sessions.len(), MAX_OPEN_SESSIONS);
        assert_eq!(
            kinds(&state)
                .iter()
                .filter(|kind| *kind == "DELETE")
                .count(),
            1,
            "the oldest was cancelled to make room"
        );
    }

    #[test]
    fn next_offset_follows_the_servers_ranges_and_refuses_the_impossible() {
        let body = |ranges: Value| {
            json!({"nextExpectedRanges": ranges})
                .to_string()
                .into_bytes()
        };
        assert_eq!(next_offset(&body(json!(["100-"])), 100, 1000), Some(100));
        assert_eq!(
            next_offset(&body(json!(["40-99", "200-"])), 100, 1000),
            Some(40)
        );
        assert_eq!(next_offset(&body(json!([])), 100, 1000), Some(100));
        assert_eq!(next_offset(b"not json", 100, 1000), Some(100));
        // Ahead of what was sent, or past the file: never.
        assert_eq!(next_offset(&body(json!(["101-"])), 100, 1000), None);
        assert_eq!(next_offset(&body(json!(["2000-"])), 2000, 1000), None);
        assert_eq!(next_offset(&body(json!(["abc-"])), 100, 1000), None);
    }

    #[test]
    fn a_local_chat_target_is_refused_for_every_session_command() {
        let state = Arc::new(State::default());
        let mut t = transport(&state, GraphCloud::Global);
        let local = ProviderResourceRef::local_preview("local");
        for command in [
            WorkspaceCommand::ContinueChatUpload {
                chat: local.clone(),
                session: UploadSessionRef::for_test(1),
            },
            WorkspaceCommand::CancelChatUpload {
                chat: local,
                session: UploadSessionRef::for_test(1),
            },
        ] {
            assert!(t.dispatch(command).is_err());
        }
        assert!(state.log.lock().unwrap().is_empty());
    }

    /// Runs a whole two-fragment session upload whose final fragment answered
    /// `final_answer` and whose read-back was `read_back`.
    fn finish_with(
        final_answer: Value,
        read_back: impl Fn(u64, &ContentHashes) -> Value,
    ) -> Result<CommandReceipt, TransportError> {
        let state = Arc::new(State::default());
        script_session(&state);
        let size = FRAGMENT * 2;
        {
            let mut fragments = state.fragments.lock().unwrap();
            fragments.push_back(accepted(FRAGMENT));
            fragments.push_back(resp(201, final_answer));
        }
        let picked = pick(size);
        let hashes =
            ContentHashes::of_reader(std::fs::File::open(picked.source.path()).unwrap()).unwrap();
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, read_back(size, &hashes)));
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        proceed(&mut t, first.session()).unwrap();
        proceed(&mut t, first.session())
    }

    fn read_back_item(size: u64, id: &str, hashes: Option<Value>) -> Value {
        let mut item = item(size);
        item["id"] = json!(id);
        if let Some(hashes) = hashes {
            item["file"] = json!({"hashes": hashes});
        }
        item
    }

    fn finished(result: Result<CommandReceipt, TransportError>) -> Option<UploadedFileHandle> {
        match result {
            Ok(CommandReceipt {
                upload: Some(UploadReceipt::Done(handle)),
                ..
            }) => Some(handle),
            Err(TransportError::Unavailable(_)) => None,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_finished_session_file_must_be_the_item_the_last_fragment_answered_with() {
        // Same id: verified, with no hash, with a matching hash.
        for hashes in [None, Some(true)] {
            let handle = finished(finish_with(json!({"id": "item-1"}), |size, local| {
                read_back_item(
                    size,
                    "item-1",
                    hashes.map(
                        |_| json!({"quickXorHash": local.quick_xor, "sha256Hash": local.sha256}),
                    ),
                )
            }))
            .expect("a finished file");
            assert!(!handle.is_recovered());
        }
        // Another item at the path: refused.
        assert!(
            finished(finish_with(json!({"id": "item-1"}), |size, _| {
                read_back_item(size, "item-2", None)
            }))
            .is_none()
        );
    }

    #[test]
    fn a_session_file_whose_size_or_hash_differs_is_refused() {
        assert!(
            finished(finish_with(json!({"id": "item-1"}), |size, _| {
                read_back_item(size + 1, "item-1", None)
            }))
            .is_none()
        );
        assert!(
            finished(finish_with(json!({"id": "item-1"}), |size, _| {
                read_back_item(
                    size,
                    "item-1",
                    Some(json!({"quickXorHash": "AAAAAAAAAAAAAAAAAAAAAAAAAAA="})),
                )
            }))
            .is_none()
        );
        assert!(
            finished(finish_with(json!({"id": "item-1"}), |size, local| {
                read_back_item(
                    size,
                    "item-1",
                    Some(json!({"quickXorHash": local.quick_xor, "sha256Hash": "00"})),
                )
            }))
            .is_none()
        );
    }

    #[test]
    fn a_last_fragment_answer_without_an_id_is_adopted_only_on_a_hash_match() {
        assert!(
            finished(finish_with(json!({}), |size, _| read_back_item(
                size, "item-1", None
            )))
            .is_none()
        );
        let handle = finished(finish_with(json!({}), |size, local| {
            read_back_item(
                size,
                "item-1",
                Some(json!({"quickXorHash": local.quick_xor})),
            )
        }))
        .expect("recovered by hash");
        assert!(handle.is_recovered(), "the user must accept it");
    }

    #[test]
    fn a_session_file_changed_while_its_last_fragment_was_in_flight_is_not_attached() {
        let state = Arc::new(State::default());
        script_session(&state);
        {
            let mut fragments = state.fragments.lock().unwrap();
            fragments.push_back(accepted(FRAGMENT));
            fragments.push_back(resp(201, json!({"id": "item-1"})));
        }
        let size = FRAGMENT * 2;
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, read_back_item(size, "item-1", None)));
        let picked = pick(size);
        let mut t = transport(&state, GraphCloud::Global);
        let first = progress(start(&mut t, &picked.source).unwrap());
        proceed(&mut t, first.session()).unwrap();
        // The file changes while the last fragment is on the wire.
        *state.grow_on_fragment.lock().unwrap() = Some(picked.source.path().to_path_buf());
        assert!(finished(proceed(&mut t, first.session())).is_none());
    }
}
