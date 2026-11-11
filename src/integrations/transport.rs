//! The narrow workspace-to-integration seam.
//!
//! Callers express user intent and synchronization interest. Adapters own
//! protocol details such as credentials, pagination, retry policies, cache
//! reconciliation, or local staging.

use crate::shared::reaction::ReactionKind;
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use url::Url;

mod calendar;
mod capabilities;
mod chat;
mod compose;
mod files;
mod instant;
mod search;
mod teams;
mod upload;

pub(crate) use self::calendar::{RemoteCalendar, RemoteCalendarEvent, RemoteCalendarProjection};
pub(crate) use self::capabilities::{Capability, RemoteCapabilities};
pub(crate) use self::chat::MentionHandle;
pub(super) use self::chat::{
    MAX_REMOTE_CHAT_ATTACHMENTS, MAX_REMOTE_CHAT_MEMBERS, MAX_REMOTE_PINNED_CHAT_MESSAGES,
    RemoteChatMemberRole,
};
pub(crate) use self::chat::{
    PresenceStatus, RemoteChatAttachment, RemoteChatConversation, RemoteChatKind, RemoteChatMember,
    RemoteChatMembersProjection, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessageQuote, RemoteChatMessagesProjection, RemoteChatProjection,
    RemoteMemberPresence, RemotePinnedChatMessagesProjection,
};
pub(crate) use self::compose::{ComposedBody, DraftMention};
pub(crate) use self::compose::{Inline, Marks, RichRun, SafeHttpsUrl};
pub(crate) use self::files::{
    RemoteDriveItem, RemoteDriveItemKind, RemoteFilePreviewPayload, RemoteFilePreviewProjection,
    RemoteFilePreviewUnavailable, RemoteFilesProjection,
};
pub(crate) use self::instant::RemoteMessageInstant;
pub(super) use self::search::{MAX_REMOTE_SEARCH_PAGES, REMOTE_SEARCH_PAGE_SIZE};
pub(crate) use self::search::{
    RemoteMessageSearchAvailability, RemoteMessageSearchProjection, RemoteSearchHit,
    RemoteSearchLocation, RemoteSearchQuery,
};
pub(crate) use self::teams::{
    RemoteChannel, RemoteChannelFilesProjection, RemoteChannelMembership,
    RemoteChannelMessagesProjection, RemoteChannelPost, RemoteChannelRepliesProjection,
    RemoteChannelReply, RemoteTeam, RemoteTeamsProjection,
};
pub(crate) use self::upload::MAX_CHAT_UPLOAD_BYTES;
pub(crate) use self::upload::ShareReport;
pub(crate) use self::upload::{
    LocalUploadSource, UPLOAD_NAME_CONFLICT, UploadPickError, UploadProgress, UploadReceipt,
    UploadSessionRef, UploadedFileHandle, validate_upload_name,
};
pub(crate) use self::upload::{MAX_CHAT_UPLOAD_SESSION_BYTES, UPLOAD_FRAGMENT_BYTES};

/// A cooperative cancellation signal for one background transport operation.
///
/// The controller owns one signal per remote worker and invalidates it before
/// replacing an account or profile. Adapters check it before beginning I/O
/// and between bounded requests; the signal intentionally cannot interrupt a
/// request that is already inside the HTTP client.
#[derive(Clone, Default)]
pub(crate) struct TransportCancellation(Arc<AtomicBool>);

impl TransportCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// An opaque correlation key for one session-local workspace intent.
///
/// The controller allocates these when an action enters its FIFO. Features can
/// retain a key to reconcile an eventual outcome, but cannot inspect the
/// provider target or manufacture a Graph resource from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WorkspaceIntentId(u64);

impl WorkspaceIntentId {
    /// Allocates an opaque key from the controller's monotonic session
    /// counter. The field deliberately remains private outside this module.
    pub(crate) const fn from_sequence(sequence: u64) -> Self {
        assert!(sequence != 0, "workspace intent identifiers start at one");
        Self(sequence)
    }

    #[cfg(test)]
    pub(crate) const fn for_test(sequence: u64) -> Self {
        Self::from_sequence(sequence)
    }
}

/// The integration family that owns every target in a workspace command.
///
/// This stays at the transport seam so app/features can carry opaque targets
/// without learning whether an identifier is a local fixture key or a Graph
/// resource address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandTargetProvider {
    LocalPreview,
    Microsoft365,
}

/// An opaque identifier for a resource at the integration boundary.
///
/// Preview data deliberately uses human-readable catalog keys, while a real
/// Microsoft 365 adapter must use provider-issued IDs. Keeping that ownership
/// fact alongside every command target prevents a future adapter from
/// mistaking a local display label for an addressable Graph resource.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProviderResourceRef {
    provider: ResourceProvider,
    identifier: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ResourceProvider {
    LocalPreview,
    Microsoft365,
}

impl fmt::Debug for ProviderResourceRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Resource IDs are not credentials, but they are provider-owned
        // identifiers and should not spread through incidental debug output.
        // A caller can only obtain the Microsoft 365 value inside the
        // integrations module through `microsoft365_id`.
        formatter
            .debug_struct("ProviderResourceRef")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl ProviderResourceRef {
    /// Creates a reference that belongs only to Teamspace's local preview.
    /// These references are valid for deterministic local staging but must
    /// never be sent to a Microsoft 365 endpoint.
    pub(crate) fn local_preview(identifier: impl Into<String>) -> Self {
        Self {
            provider: ResourceProvider::LocalPreview,
            identifier: identifier.into(),
        }
    }

    /// Creates a reference backed by a provider-issued Microsoft 365 ID.
    ///
    /// Callers should create this only from a validated remote projection; a
    /// raw display label, fixture key, or locally allocated ID is not a
    /// Microsoft 365 identifier merely because it fits the string shape.
    pub(super) fn microsoft365(identifier: impl Into<String>) -> Self {
        Self {
            provider: ResourceProvider::Microsoft365,
            identifier: identifier.into(),
        }
    }

    /// Returns the provider-issued identifier only for a Microsoft 365
    /// reference. A Graph adapter must still call
    /// [`WorkspaceCommand::validate_for_microsoft365`] before using this
    /// value, so malformed command content cannot bypass transport
    /// validation.
    pub(super) fn microsoft365_id(&self) -> Option<&str> {
        (self.provider == ResourceProvider::Microsoft365).then_some(self.identifier.as_str())
    }

    /// A process-local, non-reversible key for this reference, for keying UI
    /// state such as keyboard focus. It is derived with a random per-process
    /// seed, so it never matches across runs, is never persisted, and does not
    /// expose the identifier.
    pub(crate) fn stable_key(&self) -> u64 {
        use std::hash::BuildHasher;
        static SEED: std::sync::OnceLock<std::collections::hash_map::RandomState> =
            std::sync::OnceLock::new();
        SEED.get_or_init(std::collections::hash_map::RandomState::new)
            .hash_one(self)
    }

    /// A keyed, truncated digest of this Microsoft 365 chat for the local mute
    /// list, so the chat ID itself is never stored. It is
    /// `SHA-256("teamspace-mute-v1" || secret || account_id || 0 || chat_id)`
    /// cut to 16 bytes; the per-profile secret keeps it from being guessed or
    /// matched across profiles. `None` for a local-preview reference.
    pub(crate) fn mute_digest(&self, secret: &[u8; 32], account_id: &str) -> Option<[u8; 16]> {
        use sha2::{Digest, Sha256};
        let chat_id = self.microsoft365_id()?;
        let mut hasher = Sha256::new();
        hasher.update(b"teamspace-mute-v1");
        hasher.update(secret);
        hasher.update(account_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(chat_id.as_bytes());
        let full = hasher.finalize();
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&full[..16]);
        Some(digest)
    }

    fn validate(&self, label: &str) -> Result<(), TransportError> {
        require_identifier(&self.identifier, label)
    }

    fn require_microsoft365(&self, label: &str) -> Result<(), TransportError> {
        if self.provider == ResourceProvider::Microsoft365 {
            return Ok(());
        }
        Err(TransportError::Unsupported(format!(
            "Microsoft Graph cannot target a local preview {label}"
        )))
    }

    const fn target_provider(&self) -> CommandTargetProvider {
        match self.provider {
            ResourceProvider::LocalPreview => CommandTargetProvider::LocalPreview,
            ResourceProvider::Microsoft365 => CommandTargetProvider::Microsoft365,
        }
    }
}

const MAX_PROVIDER_BROWSER_URL_BYTES: usize = 8 * 1024;

/// An opaque, provider-supplied browser destination.
///
/// Microsoft 365 adapters may construct this only after the URL has passed
/// the bounded HTTPS policy below. Features may carry it for a direct browser
/// handoff, but cannot manufacture one from display text or a local path.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProviderBrowserUrl(String);

impl fmt::Debug for ProviderBrowserUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A Graph `webUrl` can be a tenant-specific sharing destination. Keep
        // it out of incidental diagnostics even though its presence is useful
        // for debugging the action availability.
        formatter
            .debug_struct("ProviderBrowserUrl")
            .field("is_present", &true)
            .finish()
    }
}

impl AsRef<str> for ProviderBrowserUrl {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl ProviderBrowserUrl {
    /// Normalizes an optional Microsoft Graph `webUrl` into the only browser
    /// destination type that may cross the integration seam. This still does
    /// not launch a browser; the app shell revalidates it at that later,
    /// explicit user-gesture boundary.
    pub(super) fn microsoft365(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_PROVIDER_BROWSER_URL_BYTES
            || value.chars().any(char::is_control)
            || !has_explicit_https_authority(trimmed)
        {
            return None;
        }
        let url = Url::parse(trimmed).ok()?;
        (url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none())
        .then(|| Self(trimmed.into()))
    }

    #[cfg(test)]
    pub(crate) fn for_test(value: &str) -> Self {
        Self::microsoft365(value)
            .expect("test browser destinations must be bounded safe HTTPS URLs")
    }
}

/// `url::Url` permits special-scheme recovery for malformed one-slash forms.
/// A provider browser destination must retain an explicit `https://`
/// authority before it can become an opaque browser URL.
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

/// Connection state suitable for an account menu or non-blocking status UI.
// The local adapter realizes only `LocalPreview`; the remaining normalized
// states are part of the provider boundary and are exercised when a remote
// adapter is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionStatus {
    LocalPreview,
    Disconnected,
    Authorizing,
    Connected(AccountIdentity),
    ReauthenticationRequired,
    Unavailable(String),
}

/// Stable account facts supplied by an authenticated remote identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountIdentity {
    pub(crate) account_id: String,
    pub(crate) display_name: String,
    pub(crate) email: Option<String>,
    pub(crate) tenant_id: Option<String>,
}

/// A caller's interest in normalized workspace data.
// Preview bootstrapping uses `Identity`; provider adapters also receive the
// full interest set without feature modules learning provider-specific APIs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SyncInterest {
    Identity,
    Chats,
    /// A user-selected direct or group chat whose bounded message history is
    /// loaded independently of the global chat directory. The target remains
    /// opaque outside the integration seam.
    ChatMessages {
        chat: ProviderResourceRef,
    },
    /// A user-selected remote chat's bounded, read-only pinned-message view.
    /// It is independent from ordinary history so opening or closing the
    /// contextual panel cannot imply a read receipt or widen chat sync.
    ChatPinnedMessages {
        chat: ProviderResourceRef,
    },
    /// A user-selected remote chat's bounded, read-only People roster. It is
    /// separate from ordinary history and pins so it can be cancelled without
    /// changing either surface's lifecycle.
    ChatMembers {
        chat: ProviderResourceRef,
    },
    /// A presence-only refresh for an open People roster. It never re-reads
    /// the roster: the adapter uses only IDs retained from the explicit open.
    ChatMemberPresence {
        chat: ProviderResourceRef,
    },
    /// One page of the user's Microsoft 365 chat-message search. It runs only
    /// from an explicit search request, is bound to its exact query and page,
    /// and is never persisted or repeated automatically.
    MessageSearch {
        query: RemoteSearchQuery,
        page: u8,
    },
    Teams,
    /// A user-selected Teams channel whose bounded root-post feed is loaded
    /// independently of the directory bootstrap. Both references remain
    /// opaque outside the integration seam.
    ChannelMessages {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
    /// A user-selected Teams channel whose backing-files-folder direct
    /// children are read separately from both the directory and root posts.
    /// The opaque selection is the only route data available outside the
    /// provider adapter.
    ChannelFiles {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
    /// One explicitly selected file from the signed-in user's current
    /// OneDrive-root metadata projection. The adapter owns content routing,
    /// redirect handling, MIME normalization, and byte/pixel caps; callers
    /// retain only an opaque item target.
    OneDriveFilePreview {
        item: ProviderResourceRef,
    },
    /// A user-selected root post whose bounded reply feed is read separately
    /// from the channel root-post listing. Every reference remains opaque
    /// outside the integration seam.
    ChannelReplies {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    },
    Calendar,
    Files,
    // Reserved for a future whole-account sync; no caller constructs it yet.
    #[allow(dead_code)]
    All,
}

/// A normalized synchronization response.
///
/// Each projection is normalized by its provider adapter before crossing this
/// seam; no Graph DTO, endpoint, or cursor can leak into feature code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceDelta {
    pub(crate) completed: Vec<SyncInterest>,
    pub(crate) identity: Option<AccountIdentity>,
    pub(crate) remote_chats: Option<RemoteChatProjection>,
    pub(crate) remote_chat_messages: Option<RemoteChatMessagesProjection>,
    pub(crate) remote_pinned_chat_messages: Option<RemotePinnedChatMessagesProjection>,
    pub(crate) remote_chat_members: Option<RemoteChatMembersProjection>,
    pub(crate) remote_message_search: Option<RemoteMessageSearchProjection>,
    pub(crate) remote_teams: Option<RemoteTeamsProjection>,
    pub(crate) remote_channel_messages: Option<RemoteChannelMessagesProjection>,
    pub(crate) remote_channel_files: Option<RemoteChannelFilesProjection>,
    pub(crate) remote_channel_replies: Option<RemoteChannelRepliesProjection>,
    pub(crate) remote_files: Option<RemoteFilesProjection>,
    pub(crate) remote_file_preview: Option<RemoteFilePreviewProjection>,
    pub(crate) remote_calendar: Option<RemoteCalendarProjection>,
    /// What the signed-in session can do beyond its required scopes. It
    /// accompanies the identity poll and is never persisted.
    pub(crate) remote_capabilities: Option<RemoteCapabilities>,
    pub(crate) notices: Vec<String>,
}

/// A user-intent command. It intentionally does not expose provider routes,
/// permission names, or request-body details to feature modules.
// Microsoft 365 currently implements chat sends, selected-channel root posts,
// text-only replies to selected root posts, and typed chat/root-post/reply
// reaction toggles. The remaining intents deliberately live at this boundary
// so their eventual adapters do not widen feature-facing APIs or duplicate
// provider validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceCommand {
    SendChatMessage {
        chat: ProviderResourceRef,
        body: ComposedBody,
        /// A typed, bounded remote-chat priority. Local-preview dispatches
        /// retain `Normal` only; Microsoft 365 serializes the exact Graph
        /// value at its adapter boundary.
        importance: RemoteChatMessageImportance,
        /// A file already uploaded by [`Self::UploadChatFile`], attached as a
        /// reference to the sender's own copy. The body may then be empty.
        attachment: Option<UploadedFileHandle>,
    },
    /// Uploads one picked local file to the signed-in user's OneDrive
    /// ("Microsoft Teams Chat Files") with a never-overwrite policy. The
    /// receipt carries an opaque [`UploadedFileHandle`]. Nothing is shared or
    /// sent by this command.
    UploadChatFile {
        chat: ProviderResourceRef,
        source: LocalUploadSource,
    },
    /// Uploads the next fragment of a session upload that
    /// [`Self::UploadChatFile`] opened for a file over 4 MiB. Each fragment is
    /// its own job, so other work and a cancellation can run between them.
    ContinueChatUpload {
        chat: ProviderResourceRef,
        session: UploadSessionRef,
    },
    /// Gives the other members of `chat` read access to an uploaded file:
    /// one `invite` per batch of members, with no notification, naming each
    /// member by directory ID. The IDs never leave the adapter job; the
    /// receipt carries counts only. It never sends a message.
    ShareUploadedFileWithChat {
        chat: ProviderResourceRef,
        upload: UploadedFileHandle,
    },
    /// Abandons a session upload and asks OneDrive to discard what it holds.
    CancelChatUpload {
        chat: ProviderResourceRef,
        session: UploadSessionRef,
    },
    /// Sends one plain-text reply that quotes one to
    /// [`MAX_REMOTE_QUOTE_SOURCES`] distinct selected Microsoft 365 chat
    /// messages. The endpoint's documented request shape has no importance
    /// field, so the feature deliberately limits quote replies to Graph's
    /// Normal priority.
    ReplyToChatMessageWithQuote {
        chat: ProviderResourceRef,
        messages: Vec<ProviderResourceRef>,
        body: String,
    },
    /// Sets or unsets one reaction on a selected Microsoft 365 chat message.
    /// Both opaque targets remain explicit because a message ID alone cannot
    /// safely identify its enclosing chat route.
    SetChatMessageReaction {
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    /// Replaces the plain-text body of one of the signed-in user's own
    /// Microsoft 365 chat messages. It needs the optional `Chat.ReadWrite`
    /// scope, is documented for the global cloud only, and is never replayed.
    EditChatMessage {
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        body: ComposedBody,
    },
    /// Soft-deletes one of the signed-in user's own Microsoft 365 chat
    /// messages for everyone in the chat. Needs `Chat.ReadWrite`.
    DeleteChatMessage {
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
    },
    /// Marks one Microsoft 365 chat read for the signed-in user. An opt-in,
    /// provider-visible read receipt that needs the optional `Chat.ReadWrite`
    /// scope. It is never retried or reconciled.
    MarkChatRead { chat: ProviderResourceRef },
    SendChannelPost {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        body: String,
    },
    ReplyToChannelPost {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        body: String,
    },
    /// Sets or unsets one reaction on a root post in an exact Microsoft 365
    /// team/channel route. A bare message reference is insufficient for
    /// Graph: it cannot safely tell a chat message from a channel root or
    /// construct the enclosing team and channel path.
    SetChannelMessageReaction {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    /// Sets or unsets one reaction on a reply in an exact Microsoft 365
    /// team/channel/root-post/reply route. All four targets are required so
    /// Graph cannot infer a generic chat or root-message route from the
    /// reply identifier.
    SetChannelMessageReplyReaction {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    SetMessageReaction {
        message: ProviderResourceRef,
        reaction: String,
        selected: bool,
    },
    CreateCalendarEvent {
        calendar: ProviderResourceRef,
        title: String,
        starts_at: String,
        ends_at: String,
        online_meeting: bool,
    },
    // Accepted by the transport contract before an upload route exists.
    #[allow(dead_code)]
    UploadFile {
        destination: ProviderResourceRef,
        file_name: String,
        size_bytes: u64,
    },
}

/// Graph's documented `replyWithQuote` maximum for `messageIds`.
pub(crate) const MAX_REMOTE_QUOTE_SOURCES: usize = 10;

impl WorkspaceCommand {
    /// Verifies only provider-independent input invariants. Adapters apply
    /// provider-specific permission and payload validation internally.
    pub(crate) fn validate(&self) -> Result<(), TransportError> {
        match self {
            Self::SendChatMessage {
                chat,
                body,
                attachment,
                ..
            } => {
                chat.validate("chat id")?;
                if attachment.is_some() {
                    // The file is the message; a body is optional.
                    if body.plain_projection().contains('\0') {
                        return Err(TransportError::InvalidCommand(
                            "message body contains a null byte".into(),
                        ));
                    }
                    return Ok(());
                }
                require_content(&body.plain_projection(), "message body")
            }
            Self::UploadChatFile { chat, source } => {
                chat.validate("chat id")?;
                validate_upload_name(source.name())
                    .map_err(|error| TransportError::InvalidCommand(error.user_message().into()))?;
                if source.size() > MAX_CHAT_UPLOAD_SESSION_BYTES {
                    return Err(TransportError::InvalidCommand(
                        UploadPickError::TooLarge.user_message().into(),
                    ));
                }
                Ok(())
            }
            Self::ContinueChatUpload { chat, .. }
            | Self::CancelChatUpload { chat, .. }
            | Self::ShareUploadedFileWithChat { chat, .. } => chat.validate("chat id"),
            Self::ReplyToChatMessageWithQuote {
                chat,
                messages,
                body,
            } => {
                chat.validate("chat id")?;
                if messages.is_empty() || messages.len() > MAX_REMOTE_QUOTE_SOURCES {
                    return Err(TransportError::InvalidCommand(format!(
                        "a quote reply needs one to {MAX_REMOTE_QUOTE_SOURCES} quoted chat messages"
                    )));
                }
                for (index, message) in messages.iter().enumerate() {
                    message.validate("quoted chat message id")?;
                    if messages[..index].contains(message) {
                        return Err(TransportError::InvalidCommand(
                            "a quote reply cannot quote the same chat message twice".into(),
                        ));
                    }
                }
                require_content(body, "quote reply body")
            }
            Self::SetChatMessageReaction { chat, message, .. } => {
                chat.validate("chat id")?;
                message.validate("chat message id")
            }
            Self::EditChatMessage {
                chat,
                message,
                body,
            } => {
                chat.validate("chat id")?;
                message.validate("chat message id")?;
                require_content(&body.plain_projection(), "message edit body")
            }
            Self::DeleteChatMessage { chat, message } => {
                chat.validate("chat id")?;
                message.validate("chat message id")
            }
            Self::MarkChatRead { chat } => chat.validate("chat id"),
            Self::SendChannelPost {
                team,
                channel,
                body,
            } => {
                team.validate("team id")?;
                channel.validate("channel id")?;
                require_content(body, "post body")
            }
            Self::ReplyToChannelPost {
                team,
                channel,
                post,
                body,
            } => {
                team.validate("team id")?;
                channel.validate("channel id")?;
                post.validate("post id")?;
                require_content(body, "reply body")
            }
            Self::SetChannelMessageReaction {
                team,
                channel,
                post,
                ..
            } => {
                team.validate("team id")?;
                channel.validate("channel id")?;
                post.validate("channel post id")
            }
            Self::SetChannelMessageReplyReaction {
                team,
                channel,
                post,
                reply,
                ..
            } => {
                team.validate("team id")?;
                channel.validate("channel id")?;
                post.validate("channel post id")?;
                reply.validate("channel reply id")
            }
            Self::SetMessageReaction {
                message, reaction, ..
            } => {
                message.validate("message id")?;
                require_identifier(reaction, "reaction")
            }
            Self::CreateCalendarEvent {
                calendar,
                title,
                starts_at,
                ends_at,
                ..
            } => {
                calendar.validate("calendar id")?;
                require_content(title, "event title")?;
                require_identifier(starts_at, "event start")?;
                require_identifier(ends_at, "event end")
            }
            Self::UploadFile {
                destination,
                file_name,
                ..
            } => {
                destination.validate("file destination")?;
                require_identifier(file_name, "file name")
            }
        }
    }

    /// Validates this intent and identifies the single adapter family allowed
    /// to execute it. A mixed local/remote command is rejected before either
    /// adapter can see it; otherwise an M365 target could accidentally become
    /// a local-preview success.
    pub(crate) fn target_provider(&self) -> Result<CommandTargetProvider, TransportError> {
        self.validate()?;
        let quote_targets: Vec<&ProviderResourceRef>;
        let targets: &[&ProviderResourceRef] = match self {
            Self::SendChatMessage { chat, .. }
            | Self::UploadChatFile { chat, .. }
            | Self::ContinueChatUpload { chat, .. }
            | Self::CancelChatUpload { chat, .. }
            | Self::ShareUploadedFileWithChat { chat, .. }
            | Self::MarkChatRead { chat } => &[chat],
            Self::ReplyToChatMessageWithQuote { chat, messages, .. } => {
                quote_targets = std::iter::once(chat).chain(messages).collect();
                &quote_targets
            }
            Self::SetChatMessageReaction { chat, message, .. }
            | Self::EditChatMessage { chat, message, .. }
            | Self::DeleteChatMessage { chat, message } => &[chat, message],
            Self::SendChannelPost { team, channel, .. } => &[team, channel],
            Self::ReplyToChannelPost {
                team,
                channel,
                post,
                ..
            } => &[team, channel, post],
            Self::SetChannelMessageReaction {
                team,
                channel,
                post,
                ..
            } => &[team, channel, post],
            Self::SetChannelMessageReplyReaction {
                team,
                channel,
                post,
                reply,
                ..
            } => &[team, channel, post, reply],
            Self::SetMessageReaction { message, .. } => &[message],
            Self::CreateCalendarEvent { calendar, .. } => &[calendar],
            Self::UploadFile { destination, .. } => &[destination],
        };
        let provider = targets
            .first()
            .expect("every workspace command has at least one target")
            .target_provider();
        if targets
            .iter()
            .any(|target| target.target_provider() != provider)
        {
            return Err(TransportError::InvalidCommand(
                "workspace command cannot mix local-preview and Microsoft 365 targets".into(),
            ));
        }
        Ok(provider)
    }

    /// Validates both generic command content and the provider ownership of
    /// every target before a Microsoft 365 adapter is permitted to use it.
    ///
    /// This is intentionally separate from [`Self::validate`]: the local
    /// preview accepts local catalog keys, whereas Graph must reject them even
    /// when they otherwise look like nonempty identifiers.
    pub(crate) fn validate_for_microsoft365(&self) -> Result<(), TransportError> {
        self.validate()?;
        match self {
            Self::SendChatMessage { chat, .. }
            | Self::UploadChatFile { chat, .. }
            | Self::ContinueChatUpload { chat, .. }
            | Self::CancelChatUpload { chat, .. }
            | Self::ShareUploadedFileWithChat { chat, .. }
            | Self::MarkChatRead { chat } => chat.require_microsoft365("chat"),
            Self::ReplyToChatMessageWithQuote { chat, messages, .. } => {
                chat.require_microsoft365("chat")?;
                messages
                    .iter()
                    .try_for_each(|message| message.require_microsoft365("quoted chat message"))
            }
            Self::SetChatMessageReaction { chat, message, .. }
            | Self::EditChatMessage { chat, message, .. }
            | Self::DeleteChatMessage { chat, message } => {
                chat.require_microsoft365("chat")?;
                message.require_microsoft365("chat message")
            }
            Self::SendChannelPost { team, channel, .. } => {
                team.require_microsoft365("team")?;
                channel.require_microsoft365("channel")
            }
            Self::ReplyToChannelPost {
                team,
                channel,
                post,
                ..
            } => {
                team.require_microsoft365("team")?;
                channel.require_microsoft365("channel")?;
                post.require_microsoft365("channel post")
            }
            Self::SetChannelMessageReaction {
                team,
                channel,
                post,
                ..
            } => {
                team.require_microsoft365("team")?;
                channel.require_microsoft365("channel")?;
                post.require_microsoft365("channel post")
            }
            Self::SetChannelMessageReplyReaction {
                team,
                channel,
                post,
                reply,
                ..
            } => {
                team.require_microsoft365("team")?;
                channel.require_microsoft365("channel")?;
                post.require_microsoft365("channel post")?;
                reply.require_microsoft365("channel reply")
            }
            Self::SetMessageReaction { message, .. } => message.require_microsoft365("message"),
            Self::CreateCalendarEvent { calendar, .. } => calendar.require_microsoft365("calendar"),
            Self::UploadFile { destination, .. } => {
                destination.require_microsoft365("file destination")
            }
        }
    }
}

fn require_identifier(value: &str, label: &str) -> Result<(), TransportError> {
    if value.trim().is_empty() {
        return Err(TransportError::InvalidCommand(format!(
            "{label} is required"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(TransportError::InvalidCommand(format!(
            "{label} contains a control character"
        )));
    }
    Ok(())
}

fn require_content(value: &str, label: &str) -> Result<(), TransportError> {
    if value.trim().is_empty() {
        return Err(TransportError::InvalidCommand(format!(
            "{label} is required"
        )));
    }
    if value.contains('\0') {
        return Err(TransportError::InvalidCommand(format!(
            "{label} contains a null byte"
        )));
    }
    Ok(())
}

/// The outcome of accepting a user command.
// A local adapter can only stage work; remote adapters select one of the
// other provider-neutral outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandDisposition {
    StagedLocally,
    // For a future durable outbox; today every adapter applies or stages.
    #[allow(dead_code)]
    QueuedForSync,
    AppliedRemotely,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandReceipt {
    pub(crate) command_id: String,
    pub(crate) disposition: CommandDisposition,
    /// The file an [`WorkspaceCommand::UploadChatFile`] stored, for the
    /// feature that asked for it. `None` for every other command.
    pub(crate) upload: Option<UploadReceipt>,
}

/// Errors whose semantics are stable across integration adapters.
// The local adapter only reaches validation/unavailability today, while the
// full taxonomy prevents a future provider adapter from leaking SDK errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
    /// The controller superseded this operation before its result could be
    /// applied. This is an expected lifecycle outcome, not a user-visible
    /// connection failure.
    Cancelled,
    // Reserved for adapters that report a missing configuration.
    #[allow(dead_code)]
    NotConfigured,
    Unauthorized,
    ReauthenticationRequired,
    Throttled {
        retry_after_seconds: Option<u64>,
    },
    Unavailable(String),
    Unsupported(String),
    InvalidCommand(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("workspace transport operation was cancelled"),
            Self::NotConfigured => formatter.write_str("workspace transport is not configured"),
            Self::Unauthorized => formatter.write_str("workspace authorization was rejected"),
            Self::ReauthenticationRequired => {
                formatter.write_str("workspace authentication must be renewed")
            }
            Self::Throttled {
                retry_after_seconds: Some(seconds),
            } => write!(
                formatter,
                "workspace transport is throttled; retry after {seconds}s"
            ),
            Self::Throttled {
                retry_after_seconds: None,
            } => formatter.write_str("workspace transport is throttled"),
            Self::Unavailable(detail) => {
                write!(formatter, "workspace transport is unavailable: {detail}")
            }
            Self::Unsupported(detail) => {
                write!(formatter, "workspace transport does not support {detail}")
            }
            Self::InvalidCommand(detail) => {
                write!(formatter, "workspace command is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

/// The one coordinator-facing integration interface.
///
/// Implementations must not block the UI thread. Callers schedule poll and
/// dispatch work off-thread, then apply returned deltas or receipts on the UI
/// thread.
pub(crate) trait WorkspaceTransport: Send {
    fn connection_status(&self) -> ConnectionStatus;

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError>;

    /// Polls with a controller-owned cooperative cancellation signal.
    ///
    /// Existing deterministic adapters can implement only [`Self::poll`]. A
    /// network adapter overrides this method to check the signal between its
    /// bounded requests, while the default still prevents a pre-cancelled
    /// operation from starting or returning a stale result.
    fn poll_cancellable(
        &mut self,
        interest: SyncInterest,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let result = self.poll(interest);
        if cancellation.is_cancelled() {
            Err(TransportError::Cancelled)
        } else {
            result
        }
    }

    fn dispatch(&mut self, command: WorkspaceCommand) -> Result<CommandReceipt, TransportError>;

    /// Dispatches with a controller-owned cooperative cancellation signal.
    ///
    /// The default maintains compatibility with deterministic adapters. A
    /// remote adapter may override it to check between protocol steps. It
    /// cannot revoke a request already inside an HTTP client, so callers must
    /// treat an invalidated remote POST as potentially delivered and never
    /// replay it automatically.
    fn dispatch_cancellable(
        &mut self,
        command: WorkspaceCommand,
        cancellation: &TransportCancellation,
    ) -> Result<CommandReceipt, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let result = self.dispatch(command);
        if cancellation.is_cancelled() {
            Err(TransportError::Cancelled)
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests;
