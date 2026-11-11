//! Replaceable workspace integrations.
//!
//! The workspace calls the small transport interface exposed here instead of
//! depending on a particular remote provider. The local-preview adapter keeps
//! tests deterministic. The optional Microsoft 365 adapter owns delegated
//! Graph identity lookup, bounded chat, Teams-directory, and OneDrive-root
//! metadata projections, an explicitly selected bounded OneDrive file preview,
//! selection-scoped chat history and pinned-message views, channel root posts,
//! replies, and backing-folder metadata, a bounded default-calendar view,
//! plaintext chat delivery,
//! and exact chat/root-post/reply reaction delivery for the app-supported
//! palette.
//! Entra
//! authorization hands an opaque,
//! in-memory credential bundle directly to that adapter; each additional
//! synchronization surface remains behind the same seam as it is implemented.

// The authorization module retains browser-facing OAuth details and raw token
// movement within integrations. App code receives only the opaque bundle.
// Some deliberately narrow future-facing helpers stay private to this module
// until refresh persistence and remote projections are implemented.
pub(crate) mod cloud;
pub(crate) mod entra_auth;
mod local_preview;
mod microsoft365;
mod reaction;
mod transport;

pub(crate) use self::entra_auth::authorization::DelegatedTokens;
pub(crate) use self::local_preview::LocalPreviewTransport;
pub(crate) use self::microsoft365::Microsoft365Transport;
#[cfg(feature = "fuzzing")]
pub(crate) use self::microsoft365::fuzz_support as microsoft365_fuzz;
pub(crate) use self::reaction::RemoteMessageReaction;
pub(crate) use self::transport::{
    Capability, CommandDisposition, CommandReceipt, CommandTargetProvider, ComposedBody,
    ConnectionStatus, DraftMention, Inline, LocalUploadSource, MAX_REMOTE_QUOTE_SOURCES, Marks,
    MentionHandle, ProviderBrowserUrl, ProviderResourceRef, RemoteCalendarProjection,
    RemoteCapabilities, RemoteChannelFilesProjection, RemoteChannelMembership,
    RemoteChannelMessagesProjection, RemoteChannelRepliesProjection, RemoteChatConversation,
    RemoteChatKind, RemoteChatMembersProjection, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, RemoteChatProjection, RemoteDriveItemKind,
    RemoteFilePreviewPayload, RemoteFilePreviewProjection, RemoteFilesProjection,
    RemoteMemberPresence, RemoteMessageInstant, RemoteMessageSearchAvailability,
    RemoteMessageSearchProjection, RemotePinnedChatMessagesProjection, RemoteSearchHit,
    RemoteSearchLocation, RemoteSearchQuery, RemoteTeamsProjection, RichRun, SyncInterest,
    TransportCancellation, TransportError, UPLOAD_NAME_CONFLICT, UploadPickError, UploadProgress,
    UploadReceipt, UploadSessionRef, UploadedFileHandle, WorkspaceCommand, WorkspaceDelta,
    WorkspaceIntentId, WorkspaceTransport,
};

#[cfg(test)]
pub(crate) use self::transport::{
    AccountIdentity, PresenceStatus, RemoteCalendar, RemoteCalendarEvent, RemoteChannel,
    RemoteChannelPost, RemoteChannelReply, RemoteChatMember, RemoteDriveItem, RemoteTeam,
    SafeHttpsUrl, ShareReport,
};
