//! Optional Microsoft 365 identity, bounded chat/Teams/OneDrive/calendar
//! reads including selected-chat history and pinned messages, an explicit
//! bounded OneDrive-root file preview, selection-scoped channel posts, replies,
//! and backing-folder metadata, text-only
//! chat/channel-message delivery, and bounded chat/root-post/reply reaction
//! delivery for the app-supported palette.
//!
//! This adapter accepts an already-issued delegated Microsoft Graph access
//! token only through the integration-owned Entra handoff. It neither
//! implements OAuth nor persists, logs, or renders the token.

mod calendar;
mod channel_files;
mod channel_messages;
mod channel_posts;
mod channel_reactions;
mod channel_replies;
mod channel_reply_posts;
mod chat_members;
mod chat_message_edits;
mod chat_presence;
mod chat_reactions;
mod chat_read_receipts;
mod chat_shares;
mod chat_upload_sessions;
mod chat_uploads;
mod chats;
mod content_hash;
mod file_preview;
mod files;
mod member_ids;
mod message_search;
mod messages;
mod mutation;
mod pinned_chat_messages;
mod quote_replies;
mod reactions;
#[cfg(test)]
mod renewal_tests;
mod rich_html;
mod teams;
mod text;

use super::cloud::GraphCloud;
use super::entra_auth::authorization::{DelegatedTokens, RefreshError, TokenRefresher};
use super::transport::{AccountIdentity, RemoteCapabilities};
use super::{
    CommandReceipt, ConnectionStatus, SyncInterest, TransportCancellation, TransportError,
    WorkspaceCommand, WorkspaceDelta, WorkspaceTransport,
};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_RANGE, CONTENT_TYPE, HeaderMap, LOCATION, RETRY_AFTER};
use reqwest::redirect::Policy;
use serde::Deserialize;
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

/// Fuzzing seam over the private Graph decoders; see `crate::fuzz`.
#[cfg(feature = "fuzzing")]
pub(crate) mod fuzz_support {
    /// Number of adapters `page` can select between.
    pub(crate) const PAGE_KINDS: u8 = 7;

    /// Decodes `body` as one adapter's collection page and returns the rows seen.
    pub(crate) fn page(kind: u8, body: &[u8]) -> usize {
        match kind % PAGE_KINDS {
            0 => super::chats::fuzz_decode(body),
            1 => super::teams::fuzz_decode(body),
            2 => super::channel_messages::fuzz_decode(body),
            3 => super::calendar::fuzz_decode(body),
            4 => super::files::fuzz_decode(body),
            5 => super::channel_files::fuzz_decode(body),
            _ => super::channel_replies::fuzz_decode(body),
        }
    }

    /// Normalizes a Graph message body exactly as chat and channel reads do.
    pub(crate) fn message_body(html: bool, raw: &str) -> String {
        super::chats::normalize_graph_message_body(html.then_some("html"), Some(raw))
    }
}

const GRAPH_ME_QUERY: &str = "$select=id,displayName,mail,userPrincipalName";
const GRAPH_TOKEN_ENVIRONMENT_VARIABLE: &str = "TEAMSPACE_GRAPH_ACCESS_TOKEN";
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_GRAPH_RESPONSE_BYTES: u64 = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// An access token this close to expiry is renewed before the next request.
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(5 * 60);
/// After a throttled or transiently failed renewal, pre-flight renewal waits
/// at least this long. A `401` always renews: it is the explicit path.
const MIN_PREFLIGHT_BACKOFF: Duration = Duration::from_secs(30);

/// A Microsoft Graph transport backed by an integration-supplied delegated token.
///
/// Use [`Self::from_environment`] only for a short-lived development token or
/// a secure process-level secret injection mechanism. The adapter never
/// writes the token to the local profile store.
pub(crate) struct Microsoft365Transport {
    session: GraphSession<'static>,
    status: ConnectionStatus,
    /// Retained only in memory after a successful `/me` result so a chat
    /// projection can mark the current user's messages without guessing from
    /// a display name or email address.
    identity: Option<AccountIdentity>,
    /// Provider IDs from the latest bounded OneDrive-root projection whose
    /// file name has an allow-listed text extension. A selected preview may
    /// use it to accept a generic `application/octet-stream` response and to
    /// apply the smaller text download cap; it never grants access by itself.
    one_drive_text_item_ids: std::collections::BTreeSet<String>,
    /// Directory-refresh-only memo of per-chat histories; see
    /// `chats::ChatHistoryCache`. Dropped on any reauthentication.
    chat_history_cache: chats::ChatHistoryCache,
    /// Member IDs from the latest explicit People opens, held only here for
    /// presence refreshes and @mention handles. See `member_ids::MemberIdMap`.
    member_ids: member_ids::MemberIdMap,
    /// Files this session uploaded for chat attachments, by opaque handle.
    /// Dropped on reauthentication; see `chat_uploads::UploadRegistry`.
    uploads: chat_uploads::UploadRegistry,
    /// Open upload sessions (files over 4 MiB), including each session's
    /// secret URL. Dropped on reauthentication; never logged.
    upload_sessions: chat_upload_sessions::UploadSessions,
    /// Set after Graph answers `403` to a presence request: no more presence
    /// POSTs until this transport is replaced.
    presence_denied: bool,
    /// The signed-in user's tenant, read lazily once for read receipts. It
    /// lives only in the adapter and never reaches features or storage.
    tenant_id: Option<String>,
}

impl Microsoft365Transport {
    /// Reads a supplied token from `TEAMSPACE_GRAPH_ACCESS_TOKEN` without
    /// retaining it anywhere outside this adapter. An unset variable means
    /// Microsoft Graph is not configured.
    pub(crate) fn from_environment() -> Result<Option<Self>, TransportError> {
        let Some(token) = std::env::var_os(GRAPH_TOKEN_ENVIRONMENT_VARIABLE) else {
            return Ok(None);
        };
        // An invalid cloud is a known configuration error, never a silent
        // fallback that would send the token to the wrong cloud's host.
        let cloud = GraphCloud::from_environment()
            .map_err(|error| TransportError::InvalidCommand(error.to_string()))?;
        let token = token.into_string().map_err(|_| {
            TransportError::InvalidCommand(
                "the supplied Microsoft Graph access token is not valid Unicode".into(),
            )
        })?;
        Self::from_access_token(token, cloud).map(Some)
    }

    /// Consumes in-memory Entra credentials into this Graph adapter.
    ///
    /// Only the integrations module can move the raw access-token allocation
    /// out of [`DelegatedTokens`]. The app and feature layers receive neither
    /// a string accessor nor a raw-token constructor.
    pub(crate) fn from_delegated_tokens(tokens: DelegatedTokens) -> Result<Self, TransportError> {
        let credentials = tokens.into_microsoft365_credentials();
        let access = AccessToken::new(credentials.access_token)?;
        let mut session_credentials =
            GraphCredentials::new(access, Some(credentials.expires_at), credentials.refresher);
        session_credentials.granted_scope = credentials.granted_scope;
        Ok(Self::with_session(GraphSession::new(
            credentials.cloud,
            session_credentials,
            Box::new(ReqwestGraphHttpClient::default()),
        )))
    }

    /// Creates an adapter from an integration-owned delegated Graph access
    /// token. The token must include the least-privileged `User.Read`
    /// permission for the documented `/me` request.
    fn from_access_token(access_token: String, cloud: GraphCloud) -> Result<Self, TransportError> {
        let access_token = AccessToken::new(access_token)?;
        // A supplied development token has no refresh token and no known
        // expiry, so it is never renewed.
        Ok(Self::with_session(GraphSession::new(
            cloud,
            GraphCredentials::unrenewable(access_token),
            // Building the blocking client starts a runtime thread and loads TLS
            // roots, so it is deferred to the first request on the worker.
            Box::new(ReqwestGraphHttpClient::default()),
        )))
    }

    fn with_session(session: GraphSession<'static>) -> Self {
        Self {
            session,
            status: ConnectionStatus::Authorizing,
            identity: None,
            one_drive_text_item_ids: std::collections::BTreeSet::new(),
            chat_history_cache: chats::ChatHistoryCache::default(),
            member_ids: member_ids::MemberIdMap::default(),
            uploads: chat_uploads::UploadRegistry::default(),
            upload_sessions: chat_upload_sessions::UploadSessions::default(),
            presence_denied: false,
            tenant_id: None,
        }
    }

    #[cfg(test)]
    fn with_client(access_token: &str, client: Box<dyn GraphHttpClient>) -> Self {
        Self::with_client_in(GraphCloud::Global, access_token, client)
    }

    #[cfg(test)]
    fn with_client_in(
        cloud: GraphCloud,
        access_token: &str,
        client: Box<dyn GraphHttpClient>,
    ) -> Self {
        Self {
            session: GraphSession::new(
                cloud,
                GraphCredentials::unrenewable(
                    AccessToken::new(access_token.into())
                        .expect("test access tokens must satisfy input validation"),
                ),
                client,
            ),
            status: ConnectionStatus::Authorizing,
            identity: None,
            one_drive_text_item_ids: std::collections::BTreeSet::new(),
            chat_history_cache: chats::ChatHistoryCache::default(),
            member_ids: member_ids::MemberIdMap::default(),
            uploads: chat_uploads::UploadRegistry::default(),
            upload_sessions: chat_upload_sessions::UploadSessions::default(),
            presence_denied: false,
            tenant_id: None,
        }
    }

    fn record_error(&mut self, error: &TransportError) {
        if matches!(error, TransportError::Cancelled) {
            return;
        }
        if matches!(error, TransportError::ReauthenticationRequired) {
            self.chat_history_cache.clear();
            self.member_ids.clear();
            self.uploads.clear();
            self.upload_sessions.clear();
        }
        self.status = match error {
            TransportError::Unauthorized => {
                ConnectionStatus::Unavailable("Microsoft Graph denied access".into())
            }
            TransportError::ReauthenticationRequired => ConnectionStatus::ReauthenticationRequired,
            TransportError::Throttled { .. } => {
                ConnectionStatus::Unavailable("Microsoft Graph is throttling requests".into())
            }
            TransportError::Unavailable(_) => {
                ConnectionStatus::Unavailable("Microsoft Graph is temporarily unavailable".into())
            }
            TransportError::NotConfigured
            | TransportError::Unsupported(_)
            | TransportError::InvalidCommand(_) => ConnectionStatus::Disconnected,
            TransportError::Cancelled => {
                unreachable!("cancelled errors return before status mapping")
            }
        };
    }

    fn poll_interest(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        match interest {
            SyncInterest::Identity => self.poll_identity(),
            SyncInterest::Chats => self.poll_chats(),
            SyncInterest::ChatMessages { chat } => self.poll_chat_messages(chat),
            SyncInterest::ChatPinnedMessages { chat } => self.poll_chat_pinned_messages(chat),
            SyncInterest::ChatMembers { chat } => self.poll_chat_members(chat),
            SyncInterest::ChatMemberPresence { chat } => self.poll_chat_member_presence(chat, None),
            SyncInterest::MessageSearch { query, page } => {
                self.poll_message_search(query, page, None)
            }
            SyncInterest::Teams => self.poll_teams(),
            SyncInterest::ChannelMessages { team, channel } => {
                self.poll_channel_messages(team, channel)
            }
            SyncInterest::ChannelFiles { team, channel } => self.poll_channel_files(team, channel),
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            } => self.poll_channel_replies(team, channel, post),
            SyncInterest::Files => self.poll_files(),
            SyncInterest::OneDriveFilePreview { item } => self.poll_one_drive_file_preview(item),
            SyncInterest::Calendar => self.poll_calendar(),
            _ => Err(TransportError::Unsupported(
                "this Microsoft Graph adapter does not yet synchronize that workspace surface"
                    .into(),
            )),
        }
    }

    fn poll_identity(&mut self) -> Result<WorkspaceDelta, TransportError> {
        let response = match self.session.get_me() {
            Ok(response) => response,
            Err(error) => {
                let error = error.into_transport_error();
                self.record_error(&error);
                return Err(error);
            }
        };
        let identity = match response.status {
            200 => parse_identity(&response.body),
            401 => Err(TransportError::ReauthenticationRequired),
            403 => Err(TransportError::Unauthorized),
            429 => Err(TransportError::Throttled {
                retry_after_seconds: response.retry_after_seconds,
            }),
            status if (500..=599).contains(&status) => Err(TransportError::Unavailable(
                "Microsoft Graph returned a server error".into(),
            )),
            _ => Err(TransportError::Unavailable(
                "Microsoft Graph rejected the identity request".into(),
            )),
        };

        match identity {
            Ok(identity) => {
                self.status = ConnectionStatus::Connected(identity.clone());
                self.identity = Some(identity.clone());
                Ok(WorkspaceDelta {
                    completed: vec![SyncInterest::Identity],
                    identity: Some(identity),
                    remote_chats: None,
                    remote_chat_messages: None,
                    remote_pinned_chat_messages: None,
                    remote_chat_members: None,
                    remote_message_search: None,
                    remote_teams: None,
                    remote_channel_messages: None,
                    remote_channel_files: None,
                    remote_channel_replies: None,
                    remote_files: None,
                    remote_file_preview: None,
                    remote_calendar: None,
                    remote_capabilities: Some(RemoteCapabilities::compute(
                        self.session.cloud(),
                        self.session.granted_scope().as_deref(),
                    )),
                    notices: Vec::new(),
                })
            }
            Err(error) => {
                self.record_error(&error);
                Err(error)
            }
        }
    }
}

impl WorkspaceTransport for Microsoft365Transport {
    fn connection_status(&self) -> ConnectionStatus {
        self.status.clone()
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        self.poll_interest(interest)
    }

    fn poll_cancellable(
        &mut self,
        interest: SyncInterest,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        self.session.set_cancellation(Some(cancellation.clone()));
        let result = match interest {
            // Chat synchronization can issue bounded follow-up requests, so
            // hand it the signal rather than waiting until the whole poll
            // has completed to discover that the account was replaced.
            SyncInterest::Chats => self.poll_chats_cancellable(cancellation),
            // A selected chat can change while its bounded history pages are
            // in flight, so the adapter checks cancellation before every page
            // and before the normalized history crosses the seam.
            SyncInterest::ChatMessages { chat } => {
                self.poll_chat_messages_cancellable(chat, cancellation)
            }
            // Pinned messages are a single selected-chat response, but the
            // selection can still change while it is in flight. Keep its
            // cancellation lifecycle independent from ordinary history.
            SyncInterest::ChatPinnedMessages { chat } => {
                self.poll_chat_pinned_messages_cancellable(chat, cancellation)
            }
            // The People roster is likewise one selected-chat response with
            // its own cancellation lifecycle.
            SyncInterest::ChatMembers { chat } => {
                self.poll_chat_members_cancellable(chat, cancellation)
            }
            SyncInterest::ChatMemberPresence { chat } => {
                self.poll_chat_member_presence(chat, Some(cancellation))
            }
            // A search can be replaced by a newer query while its one
            // request is in flight; the stale page must not surface.
            SyncInterest::MessageSearch { query, page } => {
                self.poll_message_search(query, page, Some(cancellation))
            }
            // The bounded Teams directory fans out from teams to their
            // channels, so it likewise observes a profile transition before
            // beginning each follow-up request.
            SyncInterest::Teams => self.poll_teams_cancellable(cancellation),
            // A selected channel may have been replaced while its first page
            // was in flight, so let the adapter check the signal before every
            // continuation and before normalized posts leave this module.
            SyncInterest::ChannelMessages { team, channel } => {
                self.poll_channel_messages_cancellable(team, channel, cancellation)
            }
            // A selected channel can be replaced while the resolver or its
            // direct-child page is in flight, so the adapter observes the
            // signal before, between, and after those bounded reads.
            SyncInterest::ChannelFiles { team, channel } => {
                self.poll_channel_files_cancellable(team, channel, cancellation)
            }
            // A selected root post can be replaced while its reply page is
            // in flight, so check the same cancellation signal before the
            // normalized feed can reach the workspace.
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            } => self.poll_channel_replies_cancellable(team, channel, post, cancellation),
            // The OneDrive-root metadata read is intentionally one bounded
            // request, but it still checks cancellation before and after I/O
            // so a profile change cannot surface a stale projection.
            SyncInterest::Files => self.poll_files_cancellable(cancellation),
            // An explicitly selected OneDrive item can be replaced while the
            // Graph redirect or its one external content request is in
            // flight. Check cancellation around both hops before a bounded
            // payload reaches the workspace.
            SyncInterest::OneDriveFilePreview { item } => {
                self.poll_one_drive_file_preview_cancellable(item, cancellation)
            }
            // Calendar synchronization may page through a bounded upcoming
            // view, so it must also stop before a stale profile starts a
            // later page request.
            SyncInterest::Calendar => self.poll_calendar_cancellable(cancellation),
            _ => self.poll_interest(interest),
        };
        self.session.set_cancellation(None);
        if cancellation.is_cancelled() {
            Err(TransportError::Cancelled)
        } else {
            result
        }
    }

    fn dispatch(&mut self, command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        // A fixture key or locally allocated identifier must never be
        // mistaken for a Graph resource ID. The provider-reference boundary
        // rejects those before any network-capable path is selected.
        command.validate_for_microsoft365()?;
        match command {
            WorkspaceCommand::SendChatMessage {
                chat,
                body,
                importance,
                attachment,
            } => self.send_chat_message(&chat, &body, importance, attachment.as_ref()),
            WorkspaceCommand::UploadChatFile { chat, source } => {
                self.upload_chat_file(&chat, &source)
            }
            WorkspaceCommand::ContinueChatUpload { chat, session } => {
                self.continue_chat_upload(&chat, session)
            }
            WorkspaceCommand::ShareUploadedFileWithChat { chat, upload } => {
                self.share_uploaded_file(&chat, &upload)
            }
            WorkspaceCommand::CancelChatUpload { chat, session } => {
                self.cancel_chat_upload(&chat, session)
            }
            WorkspaceCommand::ReplyToChatMessageWithQuote {
                chat,
                messages,
                body,
            } => self.reply_to_chat_message_with_quote(&chat, &messages, &body),
            WorkspaceCommand::EditChatMessage {
                chat,
                message,
                body,
            } => self.edit_chat_message(&chat, &message, &body),
            WorkspaceCommand::DeleteChatMessage { chat, message } => {
                self.delete_chat_message(&chat, &message)
            }
            WorkspaceCommand::MarkChatRead { chat } => self.mark_chat_read(&chat),
            WorkspaceCommand::SetChatMessageReaction {
                chat,
                message,
                reaction,
                selected,
            } => self.set_chat_message_reaction(&chat, &message, reaction, selected),
            WorkspaceCommand::SendChannelPost {
                team,
                channel,
                body,
            } => self.send_channel_post(&team, &channel, &body),
            WorkspaceCommand::ReplyToChannelPost {
                team,
                channel,
                post,
                body,
            } => self.send_channel_reply(&team, &channel, &post, &body),
            WorkspaceCommand::SetChannelMessageReaction {
                team,
                channel,
                post,
                reaction,
                selected,
            } => self.set_channel_message_reaction(&team, &channel, &post, reaction, selected),
            WorkspaceCommand::SetChannelMessageReplyReaction {
                team,
                channel,
                post,
                reply,
                reaction,
                selected,
            } => self.set_channel_message_reply_reaction(
                &team, &channel, &post, &reply, reaction, selected,
            ),
            _ => Err(TransportError::Unsupported(
                "this Microsoft Graph adapter does not support that workspace mutation".into(),
            )),
        }
    }
}

/// The session's current access token, when it lapses, and the in-memory
/// means of renewing it. A development token has neither expiry nor refresher.
struct GraphCredentials {
    access: AccessToken,
    expires_at: Option<Instant>,
    refresher: Option<TokenRefresher>,
    /// No pre-flight renewal before this instant.
    preflight_backoff_until: Option<Instant>,
    /// The scope Entra last reported for the current access token. It feeds
    /// capability gating only; a development token has none.
    granted_scope: Option<String>,
}

impl GraphCredentials {
    const fn new(
        access: AccessToken,
        expires_at: Option<Instant>,
        refresher: Option<TokenRefresher>,
    ) -> Self {
        Self {
            access,
            expires_at,
            refresher,
            preflight_backoff_until: None,
            granted_scope: None,
        }
    }

    const fn unrenewable(access: AccessToken) -> Self {
        Self::new(access, None, None)
    }
}

/// Whether a token expiring at `expires_at` should be renewed at `now`.
fn refresh_due(expires_at: Instant, now: Instant) -> bool {
    now + TOKEN_REFRESH_SKEW >= expires_at
}

enum RenewalOutcome {
    /// New credentials are in place.
    Renewed,
    /// No refresher, or the refresh token was rejected and dropped. The
    /// session cannot renew itself and the user must sign in again.
    Unrenewable,
    Throttled(Option<u64>),
    /// A transient failure (or cancellation) before any new token arrived.
    /// The refresher is kept.
    Unavailable,
}

/// Everything one Graph connection needs: the cloud its token was issued by,
/// the credentials, and the HTTP seam. The cloud is fixed for the session and
/// every route is derived from it.
///
/// The credentials sit behind a mutex held for a request and any renewal it
/// triggers, and only the serial worker that owns the adapter calls in.
/// Renewal runs before a request that is near expiry; a read that still gets
/// `401` renews and retries once. A mutation is never replayed: after a `401`
/// the session renews and reports that the message was not delivered.
struct GraphSession<'c> {
    cloud: GraphCloud,
    credentials: Mutex<GraphCredentials>,
    cancellation: Mutex<Option<TransportCancellation>>,
    client: Box<dyn GraphHttpClient + 'c>,
}

impl<'c> GraphSession<'c> {
    fn new(
        cloud: GraphCloud,
        credentials: GraphCredentials,
        client: Box<dyn GraphHttpClient + 'c>,
    ) -> Self {
        Self {
            cloud,
            credentials: Mutex::new(credentials),
            cancellation: Mutex::new(None),
            client,
        }
    }

    /// A tokenless session for fetch helpers under test.
    #[cfg(test)]
    fn for_test<T: GraphHttpClient + Sync>(client: &'c T) -> Self {
        Self::for_test_in(GraphCloud::Global, client)
    }

    #[cfg(test)]
    fn for_test_in<T: GraphHttpClient + Sync>(cloud: GraphCloud, client: &'c T) -> Self {
        Self::new(
            cloud,
            GraphCredentials::unrenewable(
                AccessToken::new("test-token".into()).expect("test token is valid"),
            ),
            Box::new(client),
        )
    }

    const fn cloud(&self) -> GraphCloud {
        self.cloud
    }

    /// Lets renewal skip its token request once the account was replaced.
    fn set_cancellation(&self, cancellation: Option<TransportCancellation>) {
        *self
            .cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cancellation;
    }

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(TransportCancellation::is_cancelled)
    }

    fn lock_credentials(&self) -> std::sync::MutexGuard<'_, GraphCredentials> {
        self.credentials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Posts one refresh-token grant and swaps the credentials on success.
    /// An Entra rejection drops (and so zeroizes) the refresher: it is never
    /// asked again this session. Tokens that arrive after a cancellation are
    /// still kept.
    fn renew(&self, credentials: &mut GraphCredentials) -> RenewalOutcome {
        if credentials.refresher.is_none() {
            return RenewalOutcome::Unrenewable;
        }
        if self.is_cancelled() {
            return RenewalOutcome::Unavailable;
        }
        let Some(refresher) = credentials.refresher.as_mut() else {
            return RenewalOutcome::Unrenewable;
        };
        match refresher.refresh() {
            Ok(renewed) => {
                let (token, expires_at, granted_scope) = renewed.into_parts();
                match AccessToken::new(token) {
                    Ok(access) => {
                        // The replaced token zeroizes when it drops.
                        drop(std::mem::replace(&mut credentials.access, access));
                        credentials.expires_at = Some(expires_at);
                        credentials.preflight_backoff_until = None;
                        // A response without a scope keeps the known one.
                        if granted_scope.is_some() {
                            credentials.granted_scope = granted_scope;
                        }
                        RenewalOutcome::Renewed
                    }
                    Err(_) => {
                        credentials.refresher = None;
                        RenewalOutcome::Unrenewable
                    }
                }
            }
            Err(RefreshError::Rejected) => {
                credentials.refresher = None;
                RenewalOutcome::Unrenewable
            }
            Err(RefreshError::Throttled {
                retry_after_seconds,
            }) => RenewalOutcome::Throttled(retry_after_seconds),
            Err(RefreshError::Unavailable) => RenewalOutcome::Unavailable,
        }
    }

    /// Renews ahead of expiry. Any failure is ignored: the request proceeds
    /// with the current token, and a resulting `401` takes the normal path.
    /// A throttled or transiently failed attempt pauses pre-flight renewal
    /// for a while so a struggling endpoint is not hit on every request.
    fn renew_if_due(&self, credentials: &mut GraphCredentials) {
        let now = Instant::now();
        if credentials.refresher.is_none()
            || credentials
                .preflight_backoff_until
                .is_some_and(|until| now < until)
            || !credentials
                .expires_at
                .is_some_and(|expires_at| refresh_due(expires_at, now))
        {
            return;
        }
        let pause = match self.renew(credentials) {
            RenewalOutcome::Throttled(retry_after_seconds) => Some(
                retry_after_seconds
                    .map_or(MIN_PREFLIGHT_BACKOFF, Duration::from_secs)
                    .max(MIN_PREFLIGHT_BACKOFF),
            ),
            RenewalOutcome::Unavailable => Some(MIN_PREFLIGHT_BACKOFF),
            RenewalOutcome::Renewed | RenewalOutcome::Unrenewable => None,
        };
        if let Some(pause) = pause {
            credentials.preflight_backoff_until = Some(now + pause);
        }
    }

    /// Sends an idempotent read, renewing and retrying exactly once on `401`.
    fn read_with_renewal<R>(
        &self,
        send: impl Fn(&str) -> Result<R, GraphHttpError>,
        status: impl Fn(&R) -> u16,
        throttled: impl FnOnce(Option<u64>) -> R,
    ) -> Result<R, GraphHttpError> {
        let mut credentials = self.lock_credentials();
        self.renew_if_due(&mut credentials);
        let first = send(credentials.access.expose())?;
        if status(&first) != 401 {
            return Ok(first);
        }
        match self.renew(&mut credentials) {
            RenewalOutcome::Renewed => send(credentials.access.expose()),
            RenewalOutcome::Unrenewable => Ok(first),
            RenewalOutcome::Throttled(retry_after_seconds) => Ok(throttled(retry_after_seconds)),
            RenewalOutcome::Unavailable => Err(GraphHttpError::Network),
        }
    }

    fn get_me(&self) -> Result<GraphHttpResponse, GraphHttpError> {
        let url = self
            .cloud
            .graph_route(&["me"], Some(GRAPH_ME_QUERY))
            .map_err(|_| GraphHttpError::Unsupported)?;
        self.read_with_renewal(
            |token| self.client.get_me(&url, token),
            |response| response.status,
            throttled_response,
        )
    }

    fn get_json(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        self.read_with_renewal(
            |token| self.client.get_json(url, token, max_response_bytes),
            |response| response.status,
            throttled_response,
        )
    }

    /// The scope Entra last reported, if any.
    fn granted_scope(&self) -> Option<String> {
        self.lock_credentials().granted_scope.clone()
    }

    /// A non-idempotent write. `send` is called at most once: no path
    /// re-sends it. A `401` renews the session for the next action but still
    /// reports non-delivery.
    fn mutate_once(
        &self,
        send: impl FnOnce(&str) -> Result<GraphHttpStatus, GraphHttpError>,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        let mut credentials = self.lock_credentials();
        self.renew_if_due(&mut credentials);
        let sent = send(credentials.access.expose())?;
        if sent.status != 401 {
            return Ok(sent);
        }
        match self.renew(&mut credentials) {
            RenewalOutcome::Renewed => Err(GraphHttpError::RenewedBeforeDelivery),
            RenewalOutcome::Unrenewable => Ok(sent),
            RenewalOutcome::Throttled(retry_after_seconds) => Ok(GraphHttpStatus {
                status: 429,
                retry_after_seconds,
            }),
            RenewalOutcome::Unavailable => Err(GraphHttpError::RenewalUnavailable),
        }
    }

    /// A read-class POST (a query that changes nothing): it follows the read
    /// path, so one `401` renews and retries exactly once. Never use it for a
    /// write.
    fn post_json_query(
        &self,
        request: &GraphJsonRequest<'_>,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        self.read_with_renewal(
            |token| {
                self.client.post_json_query(
                    GraphJsonRequest {
                        url: request.url,
                        content_type: request.content_type,
                        body: request.body,
                    },
                    token,
                    max_response_bytes,
                )
            },
            |response| response.status,
            throttled_response,
        )
    }

    fn post_json_status(
        &self,
        request: GraphJsonRequest<'_>,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        self.mutate_once(|token| self.client.post_json_status(request, token))
    }

    /// A POST that creates something, never replayed: a `401` renews the
    /// session for the next action but still reports non-delivery.
    fn post_json_once(
        &self,
        request: &GraphJsonRequest<'_>,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let mut credentials = self.lock_credentials();
        self.renew_if_due(&mut credentials);
        let sent = self.client.post_json_response(
            GraphJsonRequest {
                url: request.url,
                content_type: request.content_type,
                body: request.body,
            },
            credentials.access.expose(),
            max_response_bytes,
        )?;
        if sent.status != 401 {
            return Ok(sent);
        }
        match self.renew(&mut credentials) {
            RenewalOutcome::Renewed => Err(GraphHttpError::RenewedBeforeDelivery),
            RenewalOutcome::Unrenewable => Ok(sent),
            RenewalOutcome::Throttled(retry_after_seconds) => {
                Ok(throttled_response(retry_after_seconds))
            }
            RenewalOutcome::Unavailable => Err(GraphHttpError::RenewalUnavailable),
        }
    }

    /// The three upload-session calls carry no Graph credential at all: the
    /// session URL is the credential. They take no lock on the credentials.
    fn put_session_fragment(
        &self,
        url: &Url,
        content_range: &str,
        body: &[u8],
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        self.client
            .put_session_fragment(url, content_range, body, max_response_bytes)
    }

    fn get_session_status(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        self.client.get_session_status(url, max_response_bytes)
    }

    fn delete_session(&self, url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
        self.client.delete_session(url)
    }

    /// One file `PUT`, never replayed: a `401` renews the session for the
    /// next action but still reports non-delivery. The bounded answer is read
    /// so the caller can tell a created file from a refusal.
    fn put_bytes_once(
        &self,
        url: &Url,
        content_type: &str,
        body: &[u8],
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let mut credentials = self.lock_credentials();
        self.renew_if_due(&mut credentials);
        let sent = self.client.put_bytes(
            url,
            content_type,
            body,
            credentials.access.expose(),
            MAX_GRAPH_RESPONSE_BYTES,
        )?;
        if sent.status != 401 {
            return Ok(sent);
        }
        match self.renew(&mut credentials) {
            RenewalOutcome::Renewed => Err(GraphHttpError::RenewedBeforeDelivery),
            RenewalOutcome::Unrenewable => Ok(sent),
            RenewalOutcome::Throttled(retry_after_seconds) => {
                Ok(throttled_response(retry_after_seconds))
            }
            RenewalOutcome::Unavailable => Err(GraphHttpError::RenewalUnavailable),
        }
    }

    fn patch_json_status(
        &self,
        request: GraphJsonRequest<'_>,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        self.mutate_once(|token| self.client.patch_json_status(request, token))
    }

    fn post_empty_status(&self, url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
        self.mutate_once(|token| self.client.post_empty_status(url, token))
    }

    fn get_content_redirect(&self, url: &Url) -> Result<GraphContentRedirect, GraphHttpError> {
        self.read_with_renewal(
            |token| self.client.get_content_redirect(url, token),
            |redirect| redirect.status,
            |retry_after_seconds| GraphContentRedirect {
                status: 429,
                retry_after_seconds,
                location: None,
            },
        )
    }

    /// Never attaches the Graph bearer token: the destination is external.
    fn get_redirected_content(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphContentResponse, GraphHttpError> {
        self.client.get_redirected_content(url, max_response_bytes)
    }
}

/// Reads a bounded answer from an upload-session host.
fn read_bounded_response(
    response: reqwest::blocking::Response,
    max_response_bytes: u64,
) -> Result<GraphHttpResponse, GraphHttpError> {
    let status = response.status().as_u16();
    let retry_after_seconds = retry_after_seconds(response.headers());
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(max_response_bytes.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|_| GraphHttpError::Network)?;
    if body.len() as u64 > max_response_bytes {
        return Err(GraphHttpError::ResponseTooLarge);
    }
    Ok(GraphHttpResponse {
        status,
        retry_after_seconds,
        body: std::mem::take(&mut *body),
    })
}

fn throttled_response(retry_after_seconds: Option<u64>) -> GraphHttpResponse {
    GraphHttpResponse {
        status: 429,
        retry_after_seconds,
        body: Vec::new(),
    }
}

/// Forwards through a shared reference so a test can keep inspecting its fake
/// client after lending it to a session.
#[cfg(test)]
impl<T: GraphHttpClient + Sync> GraphHttpClient for &T {
    fn get_me(&self, url: &Url, access_token: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).get_me(url, access_token)
    }

    fn get_json(
        &self,
        url: &Url,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).get_json(url, access_token, max_response_bytes)
    }

    fn post_json_status(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        (**self).post_json_status(request, access_token)
    }

    fn put_bytes(
        &self,
        url: &Url,
        content_type: &str,
        body: &[u8],
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).put_bytes(url, content_type, body, access_token, max_response_bytes)
    }

    fn post_json_response(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).post_json_response(request, access_token, max_response_bytes)
    }

    fn put_session_fragment(
        &self,
        url: &Url,
        content_range: &str,
        body: &[u8],
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).put_session_fragment(url, content_range, body, max_response_bytes)
    }

    fn get_session_status(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).get_session_status(url, max_response_bytes)
    }

    fn delete_session(&self, url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
        (**self).delete_session(url)
    }

    fn post_json_query(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        (**self).post_json_query(request, access_token, max_response_bytes)
    }

    fn patch_json_status(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        (**self).patch_json_status(request, access_token)
    }

    fn post_empty_status(
        &self,
        url: &Url,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        (**self).post_empty_status(url, access_token)
    }

    fn get_content_redirect(
        &self,
        url: &Url,
        access_token: &str,
    ) -> Result<GraphContentRedirect, GraphHttpError> {
        (**self).get_content_redirect(url, access_token)
    }

    fn get_redirected_content(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphContentResponse, GraphHttpError> {
        (**self).get_redirected_content(url, max_response_bytes)
    }
}

struct AccessToken(String);

impl AccessToken {
    fn new(mut value: String) -> Result<Self, TransportError> {
        if value.is_empty()
            || value != value.trim()
            || value.len() > MAX_ACCESS_TOKEN_BYTES
            || value.chars().any(char::is_control)
        {
            value.zeroize();
            return Err(TransportError::InvalidCommand(
                "the supplied Microsoft Graph access token is invalid".into(),
            ));
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }

    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for AccessToken {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// Builds its `reqwest` client lazily on the first request. Construction is
/// not free (runtime thread plus TLS roots), and the adapter is created on
/// the UI thread during sign-in, while every request runs on the worker.
#[derive(Default)]
struct ReqwestGraphHttpClient {
    client: std::sync::OnceLock<Option<Client>>,
}

impl ReqwestGraphHttpClient {
    fn client(&self) -> Result<&Client, GraphHttpError> {
        self.client
            .get_or_init(|| {
                Client::builder()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .timeout(REQUEST_TIMEOUT)
                    .redirect(Policy::none())
                    .user_agent("teamspace/0.1")
                    .build()
                    .ok()
            })
            .as_ref()
            .ok_or(GraphHttpError::Unsupported)
    }
}

impl GraphHttpClient for ReqwestGraphHttpClient {
    fn get_me(&self, url: &Url, access_token: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        let response = self
            .client()?
            .get(url.as_str())
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token)
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(MAX_GRAPH_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| GraphHttpError::Network)?;
        if body.len() as u64 > MAX_GRAPH_RESPONSE_BYTES {
            return Err(GraphHttpError::ResponseTooLarge);
        }
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds,
            body: std::mem::take(&mut *body),
        })
    }

    fn get_json(
        &self,
        url: &Url,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let response = self
            .client()?
            .get(url.as_str())
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token)
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(max_response_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| GraphHttpError::Network)?;
        if body.len() as u64 > max_response_bytes {
            return Err(GraphHttpError::ResponseTooLarge);
        }
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds,
            body: std::mem::take(&mut *body),
        })
    }

    fn post_json_query(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let response = self
            .client()?
            .post(request.url.as_str())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, request.content_type)
            .bearer_auth(access_token)
            .body(request.body.to_vec())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(max_response_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| GraphHttpError::Network)?;
        if body.len() as u64 > max_response_bytes {
            return Err(GraphHttpError::ResponseTooLarge);
        }
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds,
            body: std::mem::take(&mut *body),
        })
    }

    fn put_bytes(
        &self,
        url: &Url,
        content_type: &str,
        body: &[u8],
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let response = self
            .client()?
            .put(url.as_str())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, content_type)
            .bearer_auth(access_token)
            // A few MiB can outlast the short request timeout of a read.
            .timeout(UPLOAD_TIMEOUT)
            .body(body.to_vec())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        let mut answer = Zeroizing::new(Vec::new());
        response
            .take(max_response_bytes.saturating_add(1))
            .read_to_end(&mut answer)
            .map_err(|_| GraphHttpError::Network)?;
        if answer.len() as u64 > max_response_bytes {
            return Err(GraphHttpError::ResponseTooLarge);
        }
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds,
            body: std::mem::take(&mut *answer),
        })
    }

    fn put_session_fragment(
        &self,
        url: &Url,
        content_range: &str,
        body: &[u8],
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        // The session URL is the credential: no bearer token, cookies or
        // referrer go with it, and no redirect is followed.
        let response = self
            .client()?
            .put(url.as_str())
            .header(ACCEPT, "application/json")
            .header(CONTENT_RANGE, content_range)
            .timeout(UPLOAD_TIMEOUT)
            .body(body.to_vec())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        read_bounded_response(response, max_response_bytes)
    }

    fn get_session_status(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        let response = self
            .client()?
            .get(url.as_str())
            .header(ACCEPT, "application/json")
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        read_bounded_response(response, max_response_bytes)
    }

    fn delete_session(&self, url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
        let response = self
            .client()?
            .delete(url.as_str())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        Ok(GraphHttpStatus {
            status: response.status().as_u16(),
            retry_after_seconds: retry_after_seconds(response.headers()),
        })
    }

    fn post_json_status(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        // Do not read the success body. Chat creation is non-idempotent, and
        // turning an already-accepted 201 into a local read/parsing failure
        // could tempt a caller to send a duplicate message.
        let response = self
            .client()?
            .post(request.url.as_str())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, request.content_type)
            .bearer_auth(access_token)
            .body(request.body.to_vec())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        Ok(GraphHttpStatus {
            status: response.status().as_u16(),
            retry_after_seconds: retry_after_seconds(response.headers()),
        })
    }

    fn patch_json_status(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        // Status only: an accepted edit must never turn into a local
        // parse failure that invites a duplicate attempt.
        let response = self
            .client()?
            .patch(request.url.as_str())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, request.content_type)
            .bearer_auth(access_token)
            .body(request.body.to_vec())
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        Ok(GraphHttpStatus {
            status: response.status().as_u16(),
            retry_after_seconds: retry_after_seconds(response.headers()),
        })
    }

    fn post_empty_status(
        &self,
        url: &Url,
        access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        let response = self
            .client()?
            .post(url.as_str())
            .header(ACCEPT, "application/json")
            .bearer_auth(access_token)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        Ok(GraphHttpStatus {
            status: response.status().as_u16(),
            retry_after_seconds: retry_after_seconds(response.headers()),
        })
    }

    fn get_content_redirect(
        &self,
        url: &Url,
        access_token: &str,
    ) -> Result<GraphContentRedirect, GraphHttpError> {
        // Redirect following remains disabled on the shared client. The
        // bearer token belongs only to Graph; the file-preview adapter must
        // explicitly validate the returned destination before issuing a
        // second unauthenticated request.
        let response = self
            .client()?
            .get(url.as_str())
            .header(ACCEPT, "*/*")
            .bearer_auth(access_token)
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_GRAPH_REDIRECT_LOCATION_BYTES)
            .map(|value| Zeroizing::new(value.into()));
        Ok(GraphContentRedirect {
            status,
            retry_after_seconds,
            location,
        })
    }

    fn get_redirected_content(
        &self,
        url: &Url,
        max_response_bytes: u64,
    ) -> Result<GraphContentResponse, GraphHttpError> {
        // Do not attach authorization, cookies, or a referring Graph URL to
        // the preauthenticated content hop. `Policy::none()` also makes an
        // unexpected second redirect a normal response rather than silently
        // widening this one-hop contract.
        let response = self
            .client()?
            .get(url.as_str())
            .header(
                ACCEPT,
                "image/*, text/plain, text/*;q=0.9, application/json;q=0.8",
            )
            .send()
            .map_err(|_| GraphHttpError::Network)?;
        let status = response.status().as_u16();
        // Error and redirect bodies from the preauthenticated host have no
        // role in the bounded preview contract. Do not allocate or parse them
        // before the adapter reduces the status to a safe category.
        if status != 200 {
            return Ok(GraphContentResponse {
                status,
                content_type: None,
                body: Vec::new(),
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= MAX_GRAPH_CONTENT_TYPE_BYTES)
            .map(Into::into);
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(max_response_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| GraphHttpError::Network)?;
        if body.len() as u64 > max_response_bytes {
            return Err(GraphHttpError::ResponseTooLarge);
        }
        Ok(GraphContentResponse {
            status,
            content_type,
            body: std::mem::take(&mut *body),
        })
    }
}

fn retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

struct GraphHttpResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    body: Vec<u8>,
}

const MAX_GRAPH_REDIRECT_LOCATION_BYTES: usize = 8 * 1024;
const MAX_GRAPH_CONTENT_TYPE_BYTES: usize = 256;

/// One status-only Graph `/content` response. The short-lived redirect URL is
/// deliberately retained in a zeroizing wrapper and has no `Debug` impl, so
/// it cannot drift into a feature projection or incidental diagnostics.
struct GraphContentRedirect {
    status: u16,
    retry_after_seconds: Option<u64>,
    location: Option<Zeroizing<String>>,
}

/// One bounded unauthenticated content-hop response. This private HTTP value
/// is normalized before the integration exposes a preview projection.
struct GraphContentResponse {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl GraphContentResponse {
    fn take_body(&mut self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::mem::take(&mut self.body))
    }
}

/// A narrow JSON POST request. Keeping the content type in this value makes
/// the adapter's wire contract visible to deterministic fakes without
/// exposing a general HTTP client to feature code.
struct GraphJsonRequest<'a> {
    url: &'a Url,
    content_type: &'static str,
    body: &'a [u8],
}

/// Status-only response for non-idempotent mutation routes. Its lack of a
/// body is intentional: successful chat sends need no response DTO to prove
/// acceptance, and callers must not depend on one for retry decisions.
struct GraphHttpStatus {
    status: u16,
    retry_after_seconds: Option<u64>,
}

impl Drop for GraphHttpResponse {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

impl Drop for GraphContentResponse {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

/// Narrow HTTP boundary retained here so Graph response normalization can be
/// tested without a network, a token, or an HTTP test server.
trait GraphHttpClient: Send {
    fn get_me(&self, url: &Url, access_token: &str) -> Result<GraphHttpResponse, GraphHttpError>;

    /// Reads an already validated Microsoft Graph URL. The chat adapter keeps
    /// opaque paging-link validation inside its implementation; callers must
    /// not pass arbitrary hosts through this HTTP seam.
    fn get_json(
        &self,
        _url: &Url,
        _access_token: &str,
        _max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Posts a JSON request to an integration-validated Graph route and
    /// returns headers/status only. Implementations must not read a success
    /// body for non-idempotent sends.
    fn post_json_status(
        &self,
        _request: GraphJsonRequest<'_>,
        _access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Posts a JSON body to an integration-validated Graph route that only
    /// reads (for example `getPresencesByUserId`) and returns a bounded body.
    /// Idempotent, so the session may retry it once after a `401` renewal.
    fn post_json_query(
        &self,
        _request: GraphJsonRequest<'_>,
        _access_token: &str,
        _max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Sends one file `PUT` to an integration-validated Graph route and
    /// returns its bounded answer. A write: never replayed.
    fn put_bytes(
        &self,
        _url: &Url,
        _content_type: &str,
        _body: &[u8],
        _access_token: &str,
        _max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Posts a JSON body to an integration-validated Graph route that creates
    /// something (an upload session) and returns its bounded answer. A write:
    /// the session never replays it. The real client shares the read POST's
    /// transport; only the retry policy differs.
    fn post_json_response(
        &self,
        request: GraphJsonRequest<'_>,
        access_token: &str,
        max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        self.post_json_query(request, access_token, max_response_bytes)
    }

    /// Uploads one fragment to an upload-session URL. The URL carries its own
    /// credential, so this NEVER attaches the Graph bearer token.
    fn put_session_fragment(
        &self,
        _url: &Url,
        _content_range: &str,
        _body: &[u8],
        _max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Reads an upload session's status (the ranges it still expects), without
    /// the bearer token.
    fn get_session_status(
        &self,
        _url: &Url,
        _max_response_bytes: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Cancels an upload session, without the bearer token.
    fn delete_session(&self, _url: &Url) -> Result<GraphHttpStatus, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Sends a JSON `PATCH` to an integration-validated Graph route and
    /// returns status and headers only. Never replayed.
    fn patch_json_status(
        &self,
        _request: GraphJsonRequest<'_>,
        _access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Sends a body-less, content-type-less `POST` (an action route such as
    /// `softDelete`) and returns status and headers only. Never replayed.
    fn post_empty_status(
        &self,
        _url: &Url,
        _access_token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Gets the authenticated Graph `/content` response without automatically
    /// following its preauthorized Location. Implementations must retain the
    /// redirect boundary so the adapter can ensure the bearer token never
    /// reaches an external host.
    fn get_content_redirect(
        &self,
        _url: &Url,
        _access_token: &str,
    ) -> Result<GraphContentRedirect, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }

    /// Reads exactly one adapter-validated preauthorized content destination.
    /// Implementations must not attach the Graph bearer token or automatically
    /// follow another redirect.
    fn get_redirected_content(
        &self,
        _url: &Url,
        _max_response_bytes: u64,
    ) -> Result<GraphContentResponse, GraphHttpError> {
        Err(GraphHttpError::Unsupported)
    }
}

#[derive(Debug)]
enum GraphHttpError {
    Network,
    ResponseTooLarge,
    Unsupported,
    /// A mutation got `401`, the session was silently renewed, and the write
    /// was deliberately not replayed. It was not delivered.
    RenewedBeforeDelivery,
    /// A mutation got `401` and the renewal itself could not complete. The
    /// write was not delivered.
    RenewalUnavailable,
}

impl GraphHttpError {
    fn into_transport_error(self) -> TransportError {
        match self {
            Self::Network | Self::RenewedBeforeDelivery | Self::RenewalUnavailable => {
                TransportError::Unavailable("could not reach Microsoft Graph".into())
            }
            Self::ResponseTooLarge => TransportError::Unavailable(
                "Microsoft Graph returned an unexpectedly large identity response".into(),
            ),
            Self::Unsupported => {
                TransportError::Unsupported("Microsoft Graph request is not available".into())
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphUser {
    id: String,
    display_name: Option<String>,
    mail: Option<String>,
    user_principal_name: Option<String>,
}

fn parse_identity(body: &[u8]) -> Result<AccountIdentity, TransportError> {
    let user = serde_json::from_slice::<GraphUser>(body).map_err(|_| {
        TransportError::Unavailable("Microsoft Graph returned an invalid identity response".into())
    })?;
    let account_id = nonempty(user.id).ok_or_else(|| {
        TransportError::Unavailable(
            "Microsoft Graph returned an incomplete identity response".into(),
        )
    })?;
    let email = user
        .mail
        .and_then(nonempty)
        .or_else(|| user.user_principal_name.and_then(nonempty));
    let display_name = user
        .display_name
        .and_then(nonempty)
        .or_else(|| email.clone())
        .unwrap_or_else(|| "Microsoft 365 user".into());
    Ok(AccountIdentity {
        account_id,
        display_name,
        email,
        // `/me` does not return a tenant identifier. Do not infer one from a
        // domain name; an authenticated tenant-aware OAuth flow can supply it.
        tenant_id: None,
    })
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().into())
}

#[cfg(test)]
mod tests {
    use crate::integrations::microsoft365::GraphHttpClient;
    use url::Url;
    #[test]
    fn reqwest_client_is_not_built_until_first_request() {
        let client = super::ReqwestGraphHttpClient::default();
        assert!(client.client.get().is_none());
        assert!(client.client().is_ok());
        assert!(client.client.get().is_some());
    }

    use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport, parse_identity};
    use crate::integrations::{
        ConnectionStatus, ProviderResourceRef, SyncInterest, TransportError, WorkspaceCommand,
        WorkspaceTransport,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct TestHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
    }

    impl TestHttpClient {
        fn with_response(response: Result<GraphHttpResponse, GraphHttpError>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from([response])),
            }
        }
    }

    impl GraphHttpClient for TestHttpClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.responses
                .lock()
                .expect("test HTTP client lock must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    fn response(status: u16, body: &str) -> GraphHttpResponse {
        GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn identity_poll_reports_capabilities_from_cloud_and_granted_scope() {
        use crate::integrations::{Capability, RemoteCapabilities};
        let me =
            r#"{"id":"a1","displayName":"Ada","mail":null,"userPrincipalName":"ada@example.test"}"#;
        // A development token has no known scope: offered, degrade on 403.
        let mut dev = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, me)))),
        );
        let delta = dev.poll(SyncInterest::Identity).expect("identity");
        assert_eq!(
            delta.remote_capabilities,
            Some(RemoteCapabilities::default())
        );

        let mut granted = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, me)))),
        );
        granted.session.lock_credentials().granted_scope = Some("User.Read chat.readwrite".into());
        let caps = granted
            .poll(SyncInterest::Identity)
            .expect("identity")
            .remote_capabilities
            .expect("capabilities accompany identity");
        assert_eq!(caps.chat_message_edit, Capability::Available);
        assert_eq!(caps.chat_message_delete, Capability::Available);

        let mut gov = Microsoft365Transport::with_client_in(
            crate::integrations::cloud::GraphCloud::UsGov,
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, me)))),
        );
        gov.session.lock_credentials().granted_scope = Some("User.Read".into());
        let caps = gov
            .poll(SyncInterest::Identity)
            .expect("identity")
            .remote_capabilities
            .expect("capabilities accompany identity");
        assert_eq!(caps.chat_message_edit, Capability::UnsupportedInCloud);
        assert_eq!(caps.chat_message_delete, Capability::NotGranted);
    }

    #[test]
    fn identity_poll_normalizes_a_delegated_graph_me_response() {
        let mut transport = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(
                200,
                r#"{"id":"a1","displayName":"Ada Lovelace","mail":null,"userPrincipalName":"ada@example.test"}"#,
            )))),
        );

        let delta = transport
            .poll(SyncInterest::Identity)
            .expect("a valid Graph identity response must synchronize");
        assert_eq!(delta.completed, [SyncInterest::Identity]);
        let identity = delta.identity.expect("identity must be included");
        assert_eq!(identity.account_id, "a1");
        assert_eq!(identity.display_name, "Ada Lovelace");
        assert_eq!(identity.email.as_deref(), Some("ada@example.test"));
        assert_eq!(identity.tenant_id, None);
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(identity) if identity.account_id == "a1"
        ));
    }

    #[test]
    fn expired_or_invalid_tokens_require_reauthentication_without_exposing_them() {
        let mut transport = Microsoft365Transport::with_client(
            "sensitive-token-value",
            Box::new(TestHttpClient::with_response(Ok(response(401, "{}")))),
        );

        let error = transport
            .poll(SyncInterest::Identity)
            .expect_err("401 must not be treated as a connected identity");
        assert_eq!(error, TransportError::ReauthenticationRequired);
        assert!(!error.to_string().contains("sensitive-token-value"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::ReauthenticationRequired
        ));
    }

    #[test]
    fn throttling_preserves_a_retry_hint_and_never_parses_the_error_body() {
        let mut transport = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(GraphHttpResponse {
                status: 429,
                retry_after_seconds: Some(31),
                body: b"{not a user payload}".to_vec(),
            }))),
        );

        assert_eq!(
            transport.poll(SyncInterest::Identity),
            Err(TransportError::Throttled {
                retry_after_seconds: Some(31)
            })
        );
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Unavailable(detail) if detail == "Microsoft Graph is throttling requests"
        ));
    }

    #[test]
    fn malformed_success_payload_is_safe_and_does_not_connect() {
        let mut transport = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, "{invalid")))),
        );

        assert_eq!(
            transport.poll(SyncInterest::Identity),
            Err(TransportError::Unavailable(
                "Microsoft Graph returned an invalid identity response".into()
            ))
        );
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Unavailable(detail) if detail == "Microsoft Graph is temporarily unavailable"
        ));
    }

    #[test]
    fn adapter_without_a_post_seam_never_claims_message_delivery() {
        let mut transport = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, "{}")))),
        );

        assert_eq!(
            transport.dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::microsoft365("chat-1"),
                body: "Hello".into(),
                importance: super::super::RemoteChatMessageImportance::Normal,
                attachment: None,
            }),
            Err(TransportError::Unavailable(
                "Microsoft Graph chat delivery is unavailable".into()
            ))
        );
    }

    #[test]
    fn identity_only_adapter_rejects_local_preview_targets_before_any_network_request() {
        let mut transport = Microsoft365Transport::with_client(
            "test-access-token",
            Box::new(TestHttpClient::with_response(Ok(response(200, "{}")))),
        );

        assert_eq!(
            transport.dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::local_preview("fixture-chat"),
                body: "This must stay local".into(),
                importance: super::super::RemoteChatMessageImportance::Normal,
                attachment: None,
            }),
            Err(TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview chat".into()
            ))
        );
    }

    #[test]
    fn identity_parser_requires_an_account_id_and_uses_principal_name_as_email_fallback() {
        assert_eq!(
            parse_identity(br#"{"id":"","displayName":"Ada"}"#),
            Err(TransportError::Unavailable(
                "Microsoft Graph returned an incomplete identity response".into()
            ))
        );
    }

    #[test]
    fn delegated_tokens_keep_their_cloud_in_the_session() {
        use crate::integrations::DelegatedTokens;
        for cloud in crate::integrations::cloud::GraphCloud::ALL {
            let transport = super::Microsoft365Transport::from_delegated_tokens(
                DelegatedTokens::for_test_in(cloud, "test-access-token"),
            )
            .expect("a valid token must create the transport");
            assert_eq!(transport.session.cloud(), cloud);
        }
    }

    #[test]
    fn identity_request_uses_the_sessions_cloud_host() {
        use crate::integrations::cloud::GraphCloud;
        use std::sync::{Arc, Mutex};
        struct RecordingClient(Arc<Mutex<Vec<String>>>);
        impl GraphHttpClient for RecordingClient {
            fn get_me(
                &self,
                url: &Url,
                _access_token: &str,
            ) -> Result<super::GraphHttpResponse, super::GraphHttpError> {
                self.0.lock().expect("lock").push(url.to_string());
                Err(super::GraphHttpError::Network)
            }
        }
        for cloud in GraphCloud::ALL {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let session = super::GraphSession::new(
                cloud,
                super::GraphCredentials::unrenewable(
                    super::AccessToken::new("test-access-token".into()).expect("token"),
                ),
                Box::new(RecordingClient(seen.clone())),
            );
            let _ = session.get_me();
            assert_eq!(
                seen.lock().expect("lock").as_slice(),
                [format!(
                    "https://{}/v1.0/me?$select=id,displayName,mail,userPrincipalName",
                    cloud.graph_host()
                )]
            );
        }
    }
}
