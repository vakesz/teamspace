mod attachments;
mod composer_markup;
mod conversation_view;
mod dialogs;
mod local_mutation;
mod mention_picker;
mod message_pane;
mod notifications;
mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;
mod prefs;
mod read_receipts;
mod remote;
mod remote_composer;
mod remote_projection;
mod render;
mod selection;
mod sidebar;
mod storage_map;
pub(crate) mod tabs;

use self::storage_map::{
    attachment_from_stored, color, message_from_stored, message_to_stored, rgb, seeded_messages,
    stored_conversation,
};

pub(crate) use self::attachments::upload_failure_reason;
use self::persistence::{
    CHAT_SCHEMA_VERSION, MAX_MESSAGE_TEXT_BYTES, StoredChat, StoredLocalConversation, StoredMessage,
};
pub(crate) use self::remote::RemoteUploadOutcome;
pub(crate) use self::remote::{
    PresenceSchedule, RemoteMutationLabel, RemoteMutationResolution, RemoteReconciliationResolution,
};
use self::remote::{RemoteChatState, RemoteSendError, RemoteSendRequest, RemoteSubmissionId};
use crate::data::workspace::workspace;
use crate::features::calls::CallMode;
use crate::integrations::{
    ProviderResourceRef, RemoteCapabilities, RemoteChatMessageImportance, WorkspaceIntentId,
};
use crate::shared::model::{ChatTab, Message, MessageAttachment, MessageReaction};
use crate::shared::reaction::ReactionKind;
use crate::ui::icon::{Icon, IconSize};
use crate::ui::link_preview::LinkPreview;
use crate::ui::message_view::MessageAction;
use crate::ui::theme;
use eframe::egui::{self, Color32};
pub(crate) use persistence::LocalChatStore;
pub(crate) use prefs::LocalChatPrefsStore;
use std::collections::BTreeMap;

const QUICK_VIEW_ROW_HEIGHT: f32 = 32.0;

/// The most recent deleted local message, held in memory so the Undo toast can
/// put it back. The delete is already persisted; undo re-inserts and persists
/// again. Never used for Microsoft 365 messages.
struct DeletedLocalMessage {
    selection: ConversationSelection,
    index: usize,
    local_id: u64,
    message: Message,
}

pub(crate) struct ChatState {
    selected: ConversationSelection,
    undo_message: Option<DeletedLocalMessage>,
    /// Link destinations the user explicitly clicked in a formatted remote
    /// message. The app drains them and revalidates each at its browser
    /// boundary; the feature never launches anything.
    pending_link_opens: Vec<String>,
    favorite_messages: Vec<MessageTimeline>,
    favorite_notes: Vec<String>,
    favorite_composers: Vec<ComposerState>,
    favorite_read: Vec<bool>,
    workspace_messages: Vec<MessageTimeline>,
    workspace_notes: Vec<String>,
    workspace_composers: Vec<ComposerState>,
    workspace_read: Vec<bool>,
    /// A session-local view of provider-normalized Microsoft 365 chats plus
    /// ephemeral remote text composers. It is never included in `StoredChat`
    /// or local persistence.
    remote_chats: RemoteChatState,
    /// One selected remote chat's bounded history read. The opaque target is
    /// session-only and travels only to the transport seam, never into a
    /// local profile or a provider command.
    pending_remote_chat_messages: Option<ProviderResourceRef>,
    /// A selection transition can make an in-flight selected-history read
    /// stale. The app drains this separately so it cancels only ChatMessages,
    /// not the chat directory, other workspace reads, or delivery commands.
    cancel_remote_chat_messages_requested: bool,
    /// One explicit selected-chat pinned-message read. It is a session-only
    /// contextual panel request, separate from message history and never a
    /// local pinned-conversation preference or persistence mutation.
    pending_remote_chat_pinned_messages: Option<ProviderResourceRef>,
    /// Closing the contextual panel or leaving its selected chat must stop
    /// only this target-bound `ChatPinnedMessages` poll.
    cancel_remote_chat_pinned_messages_requested: bool,
    /// One explicit selected-chat People roster read. Like pins, it is a
    /// session-only contextual request, never history, persistence, or a
    /// membership-management route.
    pending_remote_chat_members: Option<ProviderResourceRef>,
    /// Closing People or leaving its chat stops only this `ChatMembers` poll.
    cancel_remote_chat_members_requested: bool,
    /// What this session may do with its own remote messages, reported with
    /// the identity poll. Session only; reset with the remote projection.
    remote_capabilities: RemoteCapabilities,
    /// A `403` on an edit or deletion marks that action `NotGranted` for the
    /// rest of the session, even if the reported capability was unknown.
    remote_edit_forbidden: bool,
    remote_delete_forbidden: bool,
    /// A `403` on a read receipt turns receipts off for the session.
    remote_mark_read_forbidden: bool,
    /// A `403` answered a file upload: attaching is off for the session.
    remote_file_attach_forbidden: bool,
    /// Device-local choices (read receipts). Kept across a chat restore and
    /// persisted by its own store; never remote data.
    prefs: prefs::ChatPrefs,
    prefs_dirty: bool,
    /// The verified account the mute digests are keyed to. Session only.
    remote_account_id: Option<String>,
    /// A truthful system notifier exists (set by the app at startup).
    os_notifications_available: bool,
    /// The "All notification settings…" item was chosen; the shell opens
    /// Settings and takes this once.
    settings_requested: bool,
    /// The chat a receipt was already sent for in the current selection view.
    read_receipt_sent: Option<ProviderResourceRef>,
    /// The open "delete for everyone" confirmation, if any.
    confirming_remote_delete: Option<RemoteDeleteConfirmation>,
    local_conversations: Vec<LocalConversation>,
    /// Stable IDs of locally authored conversations shown in Favorites.
    /// Source favorites remain immutable fixture data and are never included.
    pinned_local_conversation_ids: Vec<u64>,
    next_local_conversation_id: u64,
    next_local_message_id: u64,
    persistence_dirty: bool,
    query: String,
    chat_search_open: bool,
    /// Transient expanded state of the Quick views, Favorites and Chats sections.
    sidebar_sections_open: [bool; 3],
    quick_view: ChatQuickView,
    tab: ChatTab,
    new_chat_open: bool,
    new_chat_focus_requested: bool,
    new_chat_recipient: String,
    new_chat_message: String,
    requested_call: Option<(String, CallMode)>,
    pending_actions: Vec<ChatAction>,
    editing_message: Option<MessageEditor>,
    editing_local_conversation: Option<LocalConversationEditor>,
    removing_local_conversation: Option<LocalConversationRemoval>,
    shared_preview: Option<MessageAttachment>,
    link_preview: Option<LinkPreview>,
    scroll_to_bottom_requested: bool,
    /// A Microsoft 365 search hit to scroll to and focus once its chat's
    /// history is loaded. Session-only; dropped when the selection moves.
    search_focus: Option<ProviderResourceRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ConversationSelection {
    Favorite(usize),
    Workspace(usize),
    Remote(usize),
    Local(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatSearchTarget {
    Favorite(usize),
    Workspace(usize),
    /// Remote snapshots are ephemeral and replacement can reorder them, so
    /// this target is valid only for the current projection/frame.
    Remote(usize),
    /// Local conversations are mutable and can be removed from the middle of
    /// the sidebar. Keep their durable ID here so a stale search result never
    /// selects whichever conversation shifted into its former index.
    Local(u64),
}

/// A provider-neutral user intent accepted by the chat experience.
///
/// Local messages update their durable preview immediately. Remote messages
/// remain an ephemeral pending composer state until their provider adapter
/// acknowledges a send and a later projection confirms its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatAction {
    SendLocalMessage {
        chat_id: String,
        body: String,
    },
    SendRemoteMessage {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        body: crate::integrations::ComposedBody,
        importance: RemoteChatMessageImportance,
        /// An already uploaded file this message carries.
        attachment: Option<crate::integrations::UploadedFileHandle>,
    },
    /// Sends the next fragment of a large file's upload session.
    ContinueRemoteUpload {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        session: crate::integrations::UploadSessionRef,
    },
    /// Stops an upload the user cancelled or that failed: the command still
    /// queued (if any) and the open session (if any).
    CancelRemoteUpload {
        chat: ProviderResourceRef,
        session: Option<crate::integrations::UploadSessionRef>,
        pending_intent: Option<WorkspaceIntentId>,
    },
    /// Uploads one picked local file for the composer of `chat`. The path
    /// inside `source` is held only for that one job.
    UploadRemoteAttachment {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        source: crate::integrations::LocalUploadSource,
    },
    /// Sends one text-only remote reply that quotes the exact opaque source
    /// rows (one to ten) selected from current Microsoft 365 history. Quote
    /// replies are always Normal priority.
    ReplyToRemoteChatMessageWithQuote {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        messages: Vec<ProviderResourceRef>,
        body: String,
    },
    /// One bounded remote chat-message reaction mutation. Both opaque targets
    /// and its typed reaction kind are retained until the app binds its
    /// controller intent; the feature never derives a Graph route from a
    /// timeline index.
    SetRemoteChatMessageReaction {
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    /// Replaces the plain-text body of one own message. Never replayed.
    EditRemoteChatMessage {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        body: crate::integrations::ComposedBody,
    },
    /// Soft-deletes one own message for everyone in the chat. Never replayed.
    DeleteRemoteChatMessage {
        submission: RemoteSubmissionId,
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
    },
}

/// A deletion awaiting the user's explicit confirmation. Both opaque targets
/// come from the exact selected-history row; the preview is display text.
struct RemoteDeleteConfirmation {
    chat: ProviderResourceRef,
    message: ProviderResourceRef,
    preview: String,
}

pub(crate) struct ChatSearchResult {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) target: ChatSearchTarget,
}

struct LocalConversation {
    id: u64,
    name: String,
    initials: String,
    color: Color32,
    messages: MessageTimeline,
    notes: String,
    composer: ComposerState,
}

/// Timeline state keeps domain messages and the provenance required for
/// local-only lifecycle actions together. The message model is shared by
/// multiple visual surfaces, so this feature-local wrapper avoids leaking
/// mutable-overlay bookkeeping into reference content or generic UI code.
#[derive(Default)]
struct MessageTimeline {
    messages: Vec<Message>,
    local_message_ids: Vec<Option<u64>>,
}

impl MessageTimeline {
    fn reference(messages: Vec<Message>) -> Self {
        let local_message_ids = vec![None; messages.len()];
        Self {
            messages,
            local_message_ids,
        }
    }

    fn push_locally_authored(&mut self, local_id: u64, message: Message) {
        debug_assert!(local_id != 0);
        self.messages.push(message);
        self.local_message_ids.push(Some(local_id));
    }

    fn local_id_at(&self, index: usize) -> Option<u64> {
        self.local_message_ids.get(index).copied().flatten()
    }

    fn as_slice(&self) -> &[Message] {
        &self.messages
    }

    fn index_for_local_id(&self, local_id: u64) -> Option<usize> {
        self.local_message_ids
            .iter()
            .position(|candidate| *candidate == Some(local_id))
    }

    fn insert_locally_authored(&mut self, index: usize, local_id: u64, message: Message) {
        let index = index.min(self.messages.len());
        self.messages.insert(index, message);
        self.local_message_ids.insert(index, Some(local_id));
    }

    /// Removes a locally authored message, returning it with the position it
    /// occupied so a deletion can be undone in place.
    fn remove_locally_authored_at(&mut self, local_id: u64) -> Option<(usize, Message)> {
        let index = self.index_for_local_id(local_id)?;
        self.local_message_ids.remove(index);
        Some((index, self.messages.remove(index)))
    }

    fn stored_messages(&self) -> Vec<StoredMessage> {
        debug_assert_eq!(self.messages.len(), self.local_message_ids.len());
        self.messages
            .iter()
            .zip(&self.local_message_ids)
            .map(|(message, local_id)| message_to_stored(message, *local_id))
            .collect()
    }

    fn from_stored(messages: Vec<StoredMessage>) -> Self {
        let mut timeline = Self::default();
        for stored in messages {
            let local_id = stored.local_id;
            timeline.messages.push(message_from_stored(stored));
            timeline.local_message_ids.push(local_id);
        }
        timeline
    }
}

struct MessageEditor {
    chat_id: String,
    local_message_id: u64,
    draft: String,
    focus_requested: bool,
}

struct LocalConversationEditor {
    id: u64,
    draft: String,
    focus_requested: bool,
}

struct LocalConversationRemoval {
    id: u64,
    name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalMessageMutationError {
    Missing,
    Empty,
    TooLong,
    IdentifierExhausted,
    NotPersistable,
}

impl LocalMessageMutationError {
    const fn user_message(self) -> &'static str {
        match self {
            Self::Missing => "That local message is no longer available.",
            Self::Empty => "A message cannot be empty.",
            Self::TooLong => "That message is too long to save locally.",
            Self::IdentifierExhausted => "This local profile cannot allocate another message ID.",
            Self::NotPersistable => "That change cannot be saved in this local profile.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalConversationMutationError {
    Missing,
    EmptyName,
    NameTooLong,
    NotPersistable,
}

impl LocalConversationMutationError {
    const fn user_message(self) -> &'static str {
        match self {
            Self::Missing => "That local chat is no longer available.",
            Self::EmptyName => "A local chat needs a name.",
            Self::NameTooLong => "That local chat name is too long to save locally.",
            Self::NotPersistable => "That local chat change cannot be saved in this profile.",
        }
    }
}

enum SendOutcome {
    Sent,
    NoContent,
    Rejected(LocalMessageMutationError),
}

enum TimelineAction {
    Message { index: usize, action: MessageAction },
    Edit(u64),
    Delete(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalConversationAction {
    Pin(u64),
    Unpin(u64),
    Rename(u64),
    Remove(u64),
}

#[derive(Default)]
struct ComposerState {
    draft: String,
    pending_attachment: Option<MessageAttachment>,
    attachment_picker_open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatQuickView {
    All,
    Mentions,
    Drafts,
}

impl ChatQuickView {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "Chats",
            Self::Mentions => "Mentions",
            Self::Drafts => "Drafts",
        }
    }

    const fn empty_message(self) -> &'static str {
        match self {
            Self::All => "No chats match your search.",
            Self::Mentions => "No mentions in your local chat preview.",
            Self::Drafts => "No conversation drafts yet.",
        }
    }
}

impl Default for ChatState {
    fn default() -> Self {
        let mut favorite_messages = Vec::with_capacity(workspace().chat.favorites.len());
        favorite_messages.resize_with(workspace().chat.favorites.len(), MessageTimeline::default);
        let favorite_notes = vec![String::new(); workspace().chat.favorites.len()];
        let mut favorite_composers = Vec::with_capacity(workspace().chat.favorites.len());
        favorite_composers.resize_with(workspace().chat.favorites.len(), ComposerState::default);
        let favorite_read = vec![false; workspace().chat.favorites.len()];
        let mut workspace_messages = vec![MessageTimeline::reference(seeded_messages())];
        workspace_messages.resize_with(
            workspace().chat.conversations.len(),
            MessageTimeline::default,
        );
        let workspace_notes = vec![String::new(); workspace().chat.conversations.len()];
        let mut workspace_composers = vec![ComposerState::default()];
        workspace_composers
            .resize_with(workspace().chat.conversations.len(), ComposerState::default);
        let mut workspace_read = vec![false; workspace().chat.conversations.len()];
        if let Some(read) = workspace_read.first_mut() {
            *read = true;
        }
        Self {
            selected: ConversationSelection::Workspace(0),
            favorite_messages,
            favorite_notes,
            favorite_composers,
            favorite_read,
            workspace_messages,
            workspace_notes,
            workspace_composers,
            workspace_read,
            remote_chats: RemoteChatState::default(),
            pending_remote_chat_messages: None,
            cancel_remote_chat_messages_requested: false,
            pending_remote_chat_pinned_messages: None,
            cancel_remote_chat_pinned_messages_requested: false,
            pending_remote_chat_members: None,
            cancel_remote_chat_members_requested: false,
            remote_capabilities: RemoteCapabilities::default(),
            remote_edit_forbidden: false,
            remote_delete_forbidden: false,
            remote_mark_read_forbidden: false,
            remote_file_attach_forbidden: false,
            prefs: prefs::ChatPrefs::default(),
            prefs_dirty: false,
            remote_account_id: None,
            os_notifications_available: false,
            settings_requested: false,
            read_receipt_sent: None,
            confirming_remote_delete: None,
            local_conversations: Vec::new(),
            pinned_local_conversation_ids: Vec::new(),
            next_local_conversation_id: 1,
            next_local_message_id: 1,
            persistence_dirty: false,
            query: String::new(),
            chat_search_open: false,
            sidebar_sections_open: [true; 3],
            quick_view: ChatQuickView::All,
            tab: ChatTab::Chat,
            new_chat_open: false,
            new_chat_focus_requested: false,
            new_chat_recipient: String::new(),
            new_chat_message: String::new(),
            requested_call: None,
            pending_actions: Vec::new(),
            editing_message: None,
            editing_local_conversation: None,
            removing_local_conversation: None,
            shared_preview: None,
            undo_message: None,
            pending_link_opens: Vec::new(),
            link_preview: None,
            scroll_to_bottom_requested: false,
            search_focus: None,
        }
    }
}

impl ChatState {
    /// Rebuilds the mutable chat overlay from a validated local snapshot.
    /// Fixture conversations are matched by their immutable IDs, so reordering
    /// `data/workspace.json` cannot attach saved data to the wrong chat.
    pub(crate) fn from_stored(stored: StoredChat) -> Self {
        let mut chat = Self::default();
        let stored_next_local_conversation_id = stored.next_local_conversation_id;
        let stored_next_local_message_id = stored.next_local_message_id;
        let stored_pinned_local_conversation_ids = stored.pinned_local_conversation_ids;
        let mut stored_conversations = stored.conversations;

        for (index, item) in workspace().chat.favorites.iter().enumerate() {
            if let Some(state) = stored_conversations.remove(&item.id) {
                chat.favorite_messages[index] = MessageTimeline::from_stored(state.messages);
                chat.favorite_notes[index] = state.notes;
                chat.favorite_composers[index] = ComposerState {
                    draft: state.draft,
                    pending_attachment: state.pending_attachment.map(attachment_from_stored),
                    attachment_picker_open: false,
                };
                chat.favorite_read[index] = state.read;
            }
        }
        for (index, item) in workspace().chat.conversations.iter().enumerate() {
            if let Some(state) = stored_conversations.remove(&item.id) {
                chat.workspace_messages[index] = MessageTimeline::from_stored(state.messages);
                chat.workspace_notes[index] = state.notes;
                chat.workspace_composers[index] = ComposerState {
                    draft: state.draft,
                    pending_attachment: state.pending_attachment.map(attachment_from_stored),
                    attachment_picker_open: false,
                };
                chat.workspace_read[index] = state.read;
            }
        }

        chat.local_conversations = stored
            .local_conversations
            .into_iter()
            .filter_map(|local| {
                (!local.name.trim().is_empty() && !local.initials.trim().is_empty()).then_some(
                    LocalConversation {
                        id: local.id,
                        name: local.name,
                        initials: local.initials,
                        color: color(local.color_rgb),
                        messages: MessageTimeline::from_stored(local.state.messages),
                        notes: local.state.notes,
                        composer: ComposerState {
                            draft: local.state.draft,
                            pending_attachment: local
                                .state
                                .pending_attachment
                                .map(attachment_from_stored),
                            attachment_picker_open: false,
                        },
                    },
                )
            })
            .collect();
        // The store validates every persisted pin before it reaches feature
        // state. Retain this defensive filter as well, so direct in-process
        // callers cannot make a stale pin render as a source-owned favorite.
        chat.pinned_local_conversation_ids = stored_pinned_local_conversation_ids
            .into_iter()
            .filter(|id| {
                chat.local_conversations
                    .iter()
                    .any(|conversation| conversation.id == *id)
            })
            .collect();
        let next_after_restored_id = chat
            .local_conversations
            .iter()
            .map(|conversation| conversation.id)
            .max()
            .map_or(1, |id| id.saturating_add(1));
        chat.next_local_conversation_id = stored_next_local_conversation_id
            .max(next_after_restored_id)
            .max(1);
        let next_after_restored_message_id = chat
            .all_local_message_ids()
            .into_iter()
            .max()
            .map_or(1, |id| id.saturating_add(1));
        chat.next_local_message_id = stored_next_local_message_id
            .max(next_after_restored_message_id)
            .max(1);
        chat
    }

    /// Captures only durable domain state. UI selection, tabs, dialogs, and
    /// other presentation state deliberately remain session-local.
    pub(crate) fn stored(&self) -> StoredChat {
        let mut conversations = BTreeMap::new();
        for (index, item) in workspace().chat.favorites.iter().enumerate() {
            conversations.insert(
                item.id.clone(),
                stored_conversation(
                    self.favorite_messages.get(index),
                    self.favorite_notes.get(index).map_or("", String::as_str),
                    self.favorite_composers.get(index),
                    self.favorite_read.get(index).copied().unwrap_or(false),
                ),
            );
        }
        for (index, item) in workspace().chat.conversations.iter().enumerate() {
            conversations.insert(
                item.id.clone(),
                stored_conversation(
                    self.workspace_messages.get(index),
                    self.workspace_notes.get(index).map_or("", String::as_str),
                    self.workspace_composers.get(index),
                    self.workspace_read.get(index).copied().unwrap_or(false),
                ),
            );
        }
        let local_conversations = self
            .local_conversations
            .iter()
            .map(|conversation| StoredLocalConversation {
                id: conversation.id,
                name: conversation.name.clone(),
                initials: conversation.initials.clone(),
                color_rgb: rgb(conversation.color),
                state: stored_conversation(
                    Some(&conversation.messages),
                    &conversation.notes,
                    Some(&conversation.composer),
                    false,
                ),
            })
            .collect();
        StoredChat {
            schema_version: CHAT_SCHEMA_VERSION,
            next_local_conversation_id: self.next_local_conversation_id,
            next_local_message_id: self.next_local_message_id,
            conversations,
            local_conversations,
            pinned_local_conversation_ids: self.pinned_local_conversation_ids.clone(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    fn mark_persistence_dirty(&mut self) {
        self.persistence_dirty = true;
    }

    pub(crate) fn has_keyboard_modal(&self) -> bool {
        self.new_chat_open
            || self.editing_message.is_some()
            || self.editing_local_conversation.is_some()
            || self.removing_local_conversation.is_some()
            || self.shared_preview.is_some()
            || self.link_preview.is_some()
            || self
                .composer_for(self.selected)
                .is_some_and(|composer| composer.attachment_picker_open)
    }

    pub(crate) fn dismiss_contextual_overlays(&mut self) {
        // A contextual pins panel must never outlive the Chat surface. This
        // also lets section navigation cancel only its selected-pin worker on
        // the following transport drain, without disturbing chat history or
        // an in-flight remote delivery reconciliation.
        self.cancel_remote_chat_pinned_messages();
        self.cancel_remote_chat_members();
        self.new_chat_open = false;
        self.new_chat_focus_requested = false;
        self.editing_message = None;
        self.editing_local_conversation = None;
        self.removing_local_conversation = None;
        self.confirming_remote_delete = None;
        self.shared_preview = None;
        self.link_preview = None;
        self.dismiss_attachment_pickers();
    }

    pub(crate) fn select_workspace_conversation(&mut self, index: usize) {
        self.select_conversation(ConversationSelection::Workspace(index));
    }

    pub(crate) fn select_remote_conversation(&mut self, index: usize) {
        self.select_conversation(ConversationSelection::Remote(index));
    }

    pub(crate) fn select_favorite_conversation(&mut self, index: usize) {
        self.select_conversation(ConversationSelection::Favorite(index));
    }

    pub(crate) fn search_results(&self, query: &str) -> Vec<ChatSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        for (index, chat) in workspace().chat.conversations.iter().enumerate() {
            if let Some(detail) = chat_search_detail(
                &query,
                &chat.name,
                &chat.preview,
                self.workspace_messages
                    .get(index)
                    .map_or(&[], |timeline| timeline.as_slice()),
                "Chat",
            ) {
                results.push(ChatSearchResult {
                    label: chat.name.clone(),
                    detail,
                    target: ChatSearchTarget::Workspace(index),
                });
            }
        }
        for (index, chat) in workspace().chat.favorites.iter().enumerate() {
            if let Some(detail) = chat_search_detail(
                &query,
                &chat.name,
                &chat.preview,
                self.favorite_messages
                    .get(index)
                    .map_or(&[], |timeline| timeline.as_slice()),
                "Favorite chat",
            ) {
                results.push(ChatSearchResult {
                    label: chat.name.clone(),
                    detail,
                    target: ChatSearchTarget::Favorite(index),
                });
            }
        }
        for (index, chat) in self.remote_chats.iter().enumerate() {
            if chat.matches_query(&query) {
                results.push(ChatSearchResult {
                    label: chat.title().into(),
                    detail: "Microsoft 365 chat".into(),
                    target: ChatSearchTarget::Remote(index),
                });
            }
        }
        for chat in &self.local_conversations {
            if let Some(detail) = chat_search_detail(
                &query,
                &chat.name,
                &chat.name,
                chat.messages.as_slice(),
                "Local preview chat",
            ) {
                results.push(ChatSearchResult {
                    label: chat.name.clone(),
                    detail,
                    target: ChatSearchTarget::Local(chat.id),
                });
            }
        }
        results
    }

    /// The selected conversation's composer, or `None` when the selection is
    /// remote or has desynced from storage.
    fn selected_composer_mut(&mut self) -> Option<&mut ComposerState> {
        match self.selected {
            ConversationSelection::Favorite(index) => self.favorite_composers.get_mut(index),
            ConversationSelection::Workspace(index) => self.workspace_composers.get_mut(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get_mut(index)
                .map(|conversation| &mut conversation.composer),
        }
    }

    fn composer_for(&self, selection: ConversationSelection) -> Option<&ComposerState> {
        match selection {
            ConversationSelection::Favorite(index) => self.favorite_composers.get(index),
            ConversationSelection::Workspace(index) => self.workspace_composers.get(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get(index)
                .map(|conversation| &conversation.composer),
        }
    }

    fn messages_for(&self, selection: ConversationSelection) -> &[Message] {
        match selection {
            ConversationSelection::Favorite(index) => self
                .favorite_messages
                .get(index)
                .map_or(&[], |timeline| timeline.as_slice()),
            ConversationSelection::Workspace(index) => self
                .workspace_messages
                .get(index)
                .map_or(&[], |timeline| timeline.as_slice()),
            ConversationSelection::Remote(index) => self
                .remote_chats
                .get(index)
                .map_or(&[], |conversation| conversation.messages()),
            ConversationSelection::Local(index) => self
                .local_conversations
                .get(index)
                .map_or(&[], |conversation| conversation.messages.as_slice()),
        }
    }

    fn timeline_for(&self, selection: ConversationSelection) -> Option<&MessageTimeline> {
        match selection {
            ConversationSelection::Favorite(index) => self.favorite_messages.get(index),
            ConversationSelection::Workspace(index) => self.workspace_messages.get(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get(index)
                .map(|conversation| &conversation.messages),
        }
    }

    fn timeline_for_mut(
        &mut self,
        selection: ConversationSelection,
    ) -> Option<&mut MessageTimeline> {
        match selection {
            ConversationSelection::Favorite(index) => self.favorite_messages.get_mut(index),
            ConversationSelection::Workspace(index) => self.workspace_messages.get_mut(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get_mut(index)
                .map(|conversation| &mut conversation.messages),
        }
    }

    fn all_local_message_ids(&self) -> Vec<u64> {
        self.favorite_messages
            .iter()
            .chain(&self.workspace_messages)
            .flat_map(|timeline| timeline.local_message_ids.iter().flatten().copied())
            .chain(self.local_conversations.iter().flat_map(|conversation| {
                conversation
                    .messages
                    .local_message_ids
                    .iter()
                    .flatten()
                    .copied()
            }))
            .collect()
    }

    /// The mutable message history, or `None` for remote (read-only) or
    /// desynced selections.
    fn messages_for_mut(&mut self, selection: ConversationSelection) -> Option<&mut Vec<Message>> {
        match selection {
            ConversationSelection::Favorite(index) => self
                .favorite_messages
                .get_mut(index)
                .map(|timeline| &mut timeline.messages),
            ConversationSelection::Workspace(index) => self
                .workspace_messages
                .get_mut(index)
                .map(|timeline| &mut timeline.messages),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get_mut(index)
                .map(|conversation| &mut conversation.messages.messages),
        }
    }

    #[cfg(test)]
    fn matches_quick_view(&self, selection: ConversationSelection) -> bool {
        let needle = self.mention_needle();
        self.matches_quick_view_with(selection, &needle)
    }

    /// Lowercase `@user` needle for the Mentions view, resolved once per
    /// frame by callers that test many rows. Empty for every other view.
    fn mention_needle(&self) -> String {
        if self.quick_view == ChatQuickView::Mentions {
            format!("@{}", workspace().current_user.to_lowercase())
        } else {
            String::new()
        }
    }

    fn matches_quick_view_with(
        &self,
        selection: ConversationSelection,
        mention_needle: &str,
    ) -> bool {
        match self.quick_view {
            ChatQuickView::All => true,
            ChatQuickView::Mentions => match selection {
                // Remote Graph messages carry a normalized current-user
                // mention bit. Do not infer a provider mention from visible
                // `@name` text: duplicate display names and HTML rendering
                // make that both inaccurate and unsafe.
                ConversationSelection::Remote(index) => self
                    .remote_chats
                    .get(index)
                    .is_some_and(|conversation| conversation.has_current_user_mention()),
                _ => self
                    .messages_for(selection)
                    .iter()
                    .any(|message| message.text.to_lowercase().contains(mention_needle)),
            },
            ChatQuickView::Drafts => match selection {
                ConversationSelection::Remote(index) => self
                    .remote_chats
                    .get(index)
                    .is_some_and(|conversation| conversation.has_draft()),
                _ => self
                    .composer_for(selection)
                    .is_some_and(|composer| !composer.draft.trim().is_empty()),
            },
        }
    }

    pub(crate) fn request_call(&mut self, participant: String, mode: CallMode) {
        self.requested_call = Some((participant, mode));
    }

    fn selected_details(&self) -> (String, String, String, Color32) {
        match self.selected {
            ConversationSelection::Favorite(index) => workspace()
                .chat
                .favorites
                .get(index)
                .map(|chat| {
                    (
                        chat.name.clone(),
                        chat.subtitle.clone(),
                        chat.initials.clone(),
                        color(chat.color),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        "Favorite chat".into(),
                        "Local preview conversation".into(),
                        "?".into(),
                        theme::brand(),
                    )
                }),
            ConversationSelection::Workspace(index) => {
                let chat = workspace()
                    .chat
                    .conversations
                    .get(index)
                    .unwrap_or_else(|| workspace().chat.primary_conversation());
                (
                    chat.name.clone(),
                    chat.subtitle.clone(),
                    chat.initials.clone(),
                    color(chat.color),
                )
            }
            ConversationSelection::Remote(index) => self
                .remote_chats
                .get(index)
                .map(|chat| {
                    (
                        chat.title().into(),
                        chat.subtitle().into(),
                        chat.initials().into(),
                        chat.color(),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        "Microsoft 365 chat".into(),
                        "Remote projection unavailable".into(),
                        "M".into(),
                        theme::brand(),
                    )
                }),
            ConversationSelection::Local(index) => self
                .local_conversations
                .get(index)
                .map(|chat| {
                    (
                        chat.name.clone(),
                        "Local preview conversation".into(),
                        chat.initials.clone(),
                        chat.color,
                    )
                })
                .unwrap_or_else(|| {
                    (
                        "New chat".into(),
                        "Local preview conversation".into(),
                        "?".into(),
                        theme::brand(),
                    )
                }),
        }
    }

    /// Returns the stable feature identity for the selected conversation.
    /// Local preview conversations use a namespaced persisted ID, never a
    /// display name, so callers can retain a stable action target.
    fn selected_chat_id(&self) -> Option<String> {
        match self.selected {
            ConversationSelection::Favorite(index) => workspace()
                .chat
                .favorites
                .get(index)
                .map(|chat| chat.id.clone()),
            ConversationSelection::Workspace(index) => workspace()
                .chat
                .conversations
                .get(index)
                .map(|chat| chat.id.clone()),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get(index)
                .map(|conversation| format!("local:{}", conversation.id)),
        }
    }

    fn enqueue_message_action(&mut self, body: String) {
        if body.is_empty() {
            return;
        }
        if let Some(chat_id) = self.selected_chat_id() {
            self.pending_actions
                .push(ChatAction::SendLocalMessage { chat_id, body });
        }
    }

    fn queue_remote_message(&mut self, index: usize) -> Result<(), RemoteSendError> {
        let request = self
            .remote_chats
            .queue_message(index, self.effective_remote_capabilities())?;
        match request {
            RemoteSendRequest::Edit {
                submission,
                target,
                message,
                body,
            } => self
                .pending_actions
                .push(ChatAction::EditRemoteChatMessage {
                    submission,
                    chat: target,
                    message,
                    body,
                }),
            RemoteSendRequest::Delete {
                submission,
                target,
                message,
            } => self
                .pending_actions
                .push(ChatAction::DeleteRemoteChatMessage {
                    submission,
                    chat: target,
                    message,
                }),
            RemoteSendRequest::Message {
                submission,
                target,
                body,
                importance,
                attachment,
            } => self.pending_actions.push(ChatAction::SendRemoteMessage {
                submission,
                chat: target,
                body,
                importance,
                attachment,
            }),
            RemoteSendRequest::QuoteReply {
                submission,
                target,
                quoted_messages,
                body,
            } => self
                .pending_actions
                .push(ChatAction::ReplyToRemoteChatMessageWithQuote {
                    submission,
                    chat: target,
                    messages: quoted_messages,
                    body,
                }),
        }
        Ok(())
    }

    /// Toggles a row in the remote quote set only after the current
    /// selected-history row rechecks as an addressable, non-deleted provider
    /// message. This is kept
    /// outside the generic local message action path so preview/local behavior
    /// cannot gain an accidental quote mutation.
    fn toggle_remote_quote_source(
        &mut self,
        remote_index: usize,
        message_index: usize,
    ) -> Result<(), RemoteSendError> {
        self.remote_chats
            .toggle_quote_source(remote_index, message_index)
    }

    /// Queues a narrow provider-neutral chat reaction only after rechecking
    /// the live selected-history projection. The UI never supplies an opaque
    /// route target, and this function never changes the rendered count or
    /// selection optimistically.
    fn queue_remote_chat_reaction(
        &mut self,
        remote_index: usize,
        message_index: usize,
        reaction: ReactionKind,
    ) -> bool {
        let Some(view) = self
            .remote_chats
            .reaction_view(remote_index, message_index, reaction)
        else {
            return false;
        };
        if !view.can_react || view.pending {
            return false;
        }
        let Some(request) =
            self.remote_chats
                .queue_reaction(remote_index, message_index, reaction, !view.selected)
        else {
            return false;
        };
        self.pending_actions
            .push(ChatAction::SetRemoteChatMessageReaction {
                chat: request.chat,
                message: request.message,
                reaction: request.reaction,
                selected: request.selected,
            });
        true
    }

    fn next_local_message_id_pair(&self) -> Option<(u64, u64)> {
        let local_id = self.next_local_message_id;
        let next_local_message_id = local_id.checked_add(1)?;
        if local_id == 0 {
            return None;
        }
        Some((local_id, next_local_message_id))
    }

    fn selected_messages(&self) -> &[Message] {
        self.messages_for(self.selected)
    }

    fn toggle_message_reaction(
        &mut self,
        message_index: usize,
        kind: ReactionKind,
    ) -> Option<bool> {
        let selected = self.selected;
        let added = {
            let message = self.messages_for_mut(selected)?.get_mut(message_index)?;
            if let Some(index) = message
                .reactions
                .iter()
                .position(|reaction| reaction.kind == kind)
            {
                if message.reactions[index].reacted_by_current_user {
                    message.reactions[index].count -= 1;
                    message.reactions[index].reacted_by_current_user = false;
                    if message.reactions[index].count == 0 {
                        message.reactions.remove(index);
                    }
                    false
                } else {
                    message.reactions[index].count += 1;
                    message.reactions[index].reacted_by_current_user = true;
                    true
                }
            } else {
                message.reactions.push(MessageReaction {
                    kind,
                    count: 1,
                    reacted_by_current_user: true,
                });
                true
            }
        };
        self.mark_persistence_dirty();
        Some(added)
    }

    /// The selected conversation's notes, or `None` when the selection is
    /// remote or has desynced from storage.
    fn selected_notes_mut(&mut self) -> Option<&mut String> {
        match self.selected {
            ConversationSelection::Favorite(index) => self.favorite_notes.get_mut(index),
            ConversationSelection::Workspace(index) => self.workspace_notes.get_mut(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => self
                .local_conversations
                .get_mut(index)
                .map(|conversation| &mut conversation.notes),
        }
    }

    fn send(&mut self) -> SendOutcome {
        let selected = self.selected;
        let Some(composer) = self.composer_for(selected) else {
            return SendOutcome::Rejected(LocalMessageMutationError::Missing);
        };
        let text = composer.draft.trim().to_owned();
        let attachment = composer.pending_attachment;
        if text.is_empty() && attachment.is_none() {
            return SendOutcome::NoContent;
        }
        if text.len() > MAX_MESSAGE_TEXT_BYTES {
            return SendOutcome::Rejected(LocalMessageMutationError::TooLong);
        }
        let Some((local_message_id, next_local_message_id)) = self.next_local_message_id_pair()
        else {
            return SendOutcome::Rejected(LocalMessageMutationError::IdentifierExhausted);
        };
        let action_body = (!text.is_empty()).then(|| text.clone());
        let message = outgoing_message(text, attachment);
        if self
            .preflight_new_local_message(
                selected,
                local_message_id,
                next_local_message_id,
                &message,
            )
            .is_err()
        {
            return SendOutcome::Rejected(LocalMessageMutationError::NotPersistable);
        }
        let Some(timeline) = self.timeline_for_mut(selected) else {
            return SendOutcome::Rejected(LocalMessageMutationError::Missing);
        };
        timeline.push_locally_authored(local_message_id, message);
        if let Some(composer) = self.selected_composer_mut() {
            composer.pending_attachment = None;
            composer.draft.clear();
            composer.attachment_picker_open = false;
        }
        self.next_local_message_id = next_local_message_id;
        if let Some(body) = action_body {
            self.enqueue_message_action(body);
        }
        self.scroll_to_bottom_requested = true;
        self.mark_persistence_dirty();
        SendOutcome::Sent
    }
}

fn outgoing_message(text: String, attachment: Option<MessageAttachment>) -> Message {
    Message {
        name: "You".into(),
        time: "Now".into(),
        text: if text.is_empty() {
            "Shared an attachment".into()
        } else {
            text
        },
        color: theme::brand(),
        own: true,
        attachment,
        day_divider_before: false,
        reply_context: None,
        reply_context_extra: 0,
        file_chips: Vec::new(),
        rich: None,
        reactions: Vec::new(),
    }
}

fn truncate_to_byte_limit(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    true
}

fn chat_search_detail(
    query: &str,
    name: &str,
    preview: &str,
    messages: &[Message],
    scope: &str,
) -> Option<String> {
    if format!("{name} {preview}").to_lowercase().contains(query) {
        return Some(format!("{scope} · {preview}"));
    }
    messages
        .iter()
        .rev()
        .find(|message| message.text.to_lowercase().contains(query))
        .map(|message| {
            format!(
                "{scope} message · {}: {}",
                message.name,
                search_snippet(&message.text)
            )
        })
}

/// Whether a fixture chat row matches the lowercase sidebar search term.
fn chat_item_matches(item: &crate::data::workspace::ChatListItem, search_term: &str) -> bool {
    search_term.is_empty()
        || format!("{} {}", item.name, item.preview)
            .to_lowercase()
            .contains(search_term)
}

/// Compact 32px sidebar row for a quick view (Mentions, Drafts).
fn quick_view_row(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), QUICK_VIEW_ROW_HEIGHT),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    if ui.is_rect_visible(rect) {
        let rounding = egui::CornerRadius::same(theme::radius::LG);
        if selected {
            ui.painter().rect_filled(rect, rounding, theme::surface());
        } else if response.hovered() {
            ui.painter()
                .rect_filled(rect, rounding, theme::surface_subtle());
        }
        let side = IconSize::S16.px();
        icon.image(IconSize::S16).tint(theme::fg2()).paint_at(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + 12.0, rect.center().y - side / 2.0),
                egui::vec2(side, side),
            ),
        );
        let font = if selected {
            theme::semibold(theme::font_size::BODY)
        } else {
            theme::regular(theme::font_size::BODY)
        };
        let galley = ui
            .painter()
            .layout_no_wrap(label.to_owned(), font, theme::fg1());
        ui.painter().galley(
            egui::pos2(
                rect.left() + 12.0 + side + 10.0,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            theme::fg1(),
        );
    }
    response
}

fn local_conversation_initials(name: &str) -> String {
    let initials = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if initials.is_empty() {
        "?".into()
    } else {
        initials
    }
}

fn search_snippet(text: &str) -> String {
    const MAX_CHARS: usize = 72;
    let mut snippet = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().nth(MAX_CHARS).is_some() {
        snippet.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests;
