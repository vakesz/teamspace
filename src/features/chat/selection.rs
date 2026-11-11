use super::ChatState;
use crate::data::workspace::workspace;
use crate::integrations::ProviderResourceRef;
use crate::shared::model::ChatTab;
use crate::ui::toast::ToastState;

use super::remote::RemoteChatHistoryState;
use super::{
    ChatAction, ChatQuickView, ChatSearchTarget, ConversationSelection, LocalConversationAction,
    LocalConversationEditor, LocalConversationRemoval,
};

impl ChatState {
    pub(crate) fn open_new_chat(&mut self) {
        self.editing_message = None;
        self.editing_local_conversation = None;
        self.removing_local_conversation = None;
        self.shared_preview = None;
        self.link_preview = None;
        self.new_chat_open = true;
        self.new_chat_focus_requested = true;
        self.new_chat_recipient.clear();
        self.new_chat_message.clear();
    }

    pub(crate) fn select_search_result(&mut self, target: ChatSearchTarget) {
        self.quick_view = ChatQuickView::All;
        self.query.clear();
        self.chat_search_open = false;
        match target {
            ChatSearchTarget::Favorite(index) => self.select_favorite_conversation(index),
            ChatSearchTarget::Workspace(index) => self.select_workspace_conversation(index),
            ChatSearchTarget::Remote(index) => self.select_remote_conversation(index),
            ChatSearchTarget::Local(id) => {
                if let Some(index) = self.local_index_for_id(id) {
                    self.select_conversation(ConversationSelection::Local(index));
                }
            }
        }
    }

    /// Opens the remote chat a Microsoft 365 search hit points at, by its
    /// opaque target. The message, when known, is scrolled to and focused once
    /// the chat's history is loaded. `false` when the chat is not in the
    /// current directory.
    pub(crate) fn open_remote_chat(
        &mut self,
        chat: &ProviderResourceRef,
        message: Option<&ProviderResourceRef>,
    ) -> bool {
        let Some(index) = self.remote_chats.index_of(chat) else {
            return false;
        };
        self.quick_view = ChatQuickView::All;
        self.query.clear();
        self.chat_search_open = false;
        self.select_remote_conversation(index);
        self.search_focus = message.cloned();
        true
    }

    /// A one-line notice when the hit's message cannot be shown: its chat's
    /// history loaded and does not contain it. Clears the pending focus then.
    pub(crate) fn take_search_focus_notice(&mut self) -> Option<&'static str> {
        let target = self.search_focus.as_ref()?;
        let ConversationSelection::Remote(index) = self.selected else {
            self.search_focus = None;
            return None;
        };
        match self.remote_chats.history_state(index)? {
            RemoteChatHistoryState::Ready
                if !self.remote_chats.has_message_target(index, target) =>
            {
                self.search_focus = None;
                Some("That message is not in the loaded history of this chat.")
            }
            RemoteChatHistoryState::AccessDenied => {
                self.search_focus = None;
                None
            }
            _ => None,
        }
    }

    pub(super) fn select_conversation(&mut self, selection: ConversationSelection) {
        let exists = match selection {
            ConversationSelection::Favorite(index) => {
                workspace().chat.favorites.get(index).is_some()
            }
            ConversationSelection::Workspace(index) => {
                workspace().chat.conversations.get(index).is_some()
            }
            ConversationSelection::Remote(index) => self.remote_chats.get(index).is_some(),
            ConversationSelection::Local(index) => self.local_conversations.get(index).is_some(),
        };
        if !exists {
            return;
        }
        let selection_changed = self.selected != selection;
        if selection_changed {
            self.search_focus = None;
        }
        // Every selection starts a new view, which may send one new receipt.
        self.read_receipt_sent = None;
        if selection_changed && matches!(self.selected, ConversationSelection::Remote(_)) {
            self.cancel_remote_chat_messages();
            self.cancel_remote_chat_pinned_messages();
            self.cancel_remote_chat_members();
        }
        self.dismiss_attachment_pickers();
        self.mark_conversation_read(selection);
        self.selected = selection;
        self.tab = ChatTab::Chat;
        self.editing_message = None;
        self.shared_preview = None;
        self.link_preview = None;
        self.scroll_to_bottom_requested = false;
        if let ConversationSelection::Remote(index) = selection {
            // A sidebar selection is an explicit read request. It is not a
            // read-receipt mutation, and it deliberately does not synthesize a
            // local message while the bounded provider history is loading.
            self.pending_remote_chat_messages = self.remote_chats.begin_history_load(index);
        }
        self.retain_dispatchable_remote_reaction_actions();
    }

    pub(super) fn cancel_remote_chat_messages(&mut self) {
        self.pending_remote_chat_messages = None;
        self.cancel_remote_chat_messages_requested = true;
        // Selection navigation cancels only the target-bound history read.
        // A non-idempotent reaction command can already be queued or in
        // flight, so preserve its bound route gate until its outcome or a
        // later history proves the provider-owned selected state. Discard
        // only a same-frame click that never reached the controller.
        self.remote_chats.clear_unbound_pending_reactions();
        self.remote_chats.clear_unbound_quote_replies();
        // This frame can contain an unbound click that the app has not yet
        // drained. Do not let it outlive the selected history that supplied
        // its target; already-bound controller intents are not present here.
        self.pending_actions
            .retain(|action| !matches!(action, ChatAction::SetRemoteChatMessageReaction { .. }));
        self.pending_actions.retain(|action| {
            !matches!(action, ChatAction::ReplyToRemoteChatMessageWithQuote { .. })
        });
    }

    /// Drops only the current contextual pinned-message panel/read. Unlike a
    /// selected `ChatMessages` history, this state has no write or delivery
    /// recovery role, so closing it always clears its safe rows before the
    /// controller cooperatively cancels any matching worker.
    pub(super) fn cancel_remote_chat_pinned_messages(&mut self) {
        let had_queued_request = self.pending_remote_chat_pinned_messages.take().is_some();
        let closed_panel = match self.selected {
            ConversationSelection::Remote(index) => self.remote_chats.close_pinned_messages(index),
            ConversationSelection::Favorite(_)
            | ConversationSelection::Workspace(_)
            | ConversationSelection::Local(_) => false,
        };
        if had_queued_request || closed_panel {
            self.cancel_remote_chat_pinned_messages_requested = true;
        }
    }

    /// Drops only the current People panel/read; closing always clears its
    /// rows before the controller cooperatively cancels any matching worker.
    pub(super) fn cancel_remote_chat_members(&mut self) {
        let had_queued_request = self.pending_remote_chat_members.take().is_some();
        let closed_panel = match self.selected {
            ConversationSelection::Remote(index) => self.remote_chats.close_members(index),
            ConversationSelection::Favorite(_)
            | ConversationSelection::Workspace(_)
            | ConversationSelection::Local(_) => false,
        };
        if had_queued_request || closed_panel {
            self.cancel_remote_chat_members_requested = true;
        }
    }

    /// Clears every transient People panel after a directory omission,
    /// disconnect, account switch, or worker loss, signalling cancellation
    /// only when a panel or request really existed.
    pub(super) fn cancel_remote_chat_members_for_projection_loss(&mut self) {
        let had_queued_request = self.pending_remote_chat_members.take().is_some();
        let closed_panel = self.remote_chats.close_all_members();
        if had_queued_request || closed_panel {
            self.cancel_remote_chat_members_requested = true;
        }
    }

    /// A directory omission, disconnect, account switch, or worker loss can
    /// invalidate more than the current selected index. Clear every transient
    /// pinned panel but mark cancellation only when a panel or request really
    /// existed, avoiding a spurious controller poll transition.
    pub(super) fn cancel_remote_chat_pinned_messages_for_projection_loss(&mut self) {
        let had_queued_request = self.pending_remote_chat_pinned_messages.take().is_some();
        let closed_panel = self.remote_chats.close_all_pinned_messages();
        if had_queued_request || closed_panel {
            self.cancel_remote_chat_pinned_messages_requested = true;
        }
    }

    /// Cancels a selected-history read after a bounded directory replacement
    /// omitted its selected chat. This is not an explicit user abandonment or
    /// account boundary: controller-bound reactions remain in the hidden
    /// target tombstone so a transient omission cannot enable a duplicate
    /// write when the chat reappears.
    pub(super) fn cancel_remote_chat_messages_for_projection_loss(&mut self) {
        self.pending_remote_chat_messages = None;
        self.cancel_remote_chat_messages_requested = true;
        self.cancel_remote_chat_pinned_messages_for_projection_loss();
        self.cancel_remote_chat_members_for_projection_loss();
        self.remote_chats.clear_unbound_pending_reactions();
        self.remote_chats.clear_unbound_quote_replies();
        self.pending_actions
            .retain(|action| !matches!(action, ChatAction::SetRemoteChatMessageReaction { .. }));
        self.pending_actions.retain(|action| {
            !matches!(action, ChatAction::ReplyToRemoteChatMessageWithQuote { .. })
        });
    }

    pub(super) fn local_index_for_id(&self, id: u64) -> Option<usize> {
        self.local_conversations
            .iter()
            .position(|conversation| conversation.id == id)
    }

    pub(super) fn selected_local_conversation_id(&self) -> Option<u64> {
        let ConversationSelection::Local(index) = self.selected else {
            return None;
        };
        self.local_conversations
            .get(index)
            .map(|conversation| conversation.id)
    }

    pub(super) fn is_local_conversation_pinned(&self, id: u64) -> bool {
        self.pinned_local_conversation_ids.contains(&id)
    }

    pub(super) fn select_local_conversation_by_id(&mut self, id: u64) {
        if let Some(index) = self.local_index_for_id(id) {
            self.select_conversation(ConversationSelection::Local(index));
        }
    }

    pub(super) fn stable_fallback_selection(&self) -> Option<ConversationSelection> {
        if !workspace().chat.conversations.is_empty() {
            Some(ConversationSelection::Workspace(0))
        } else if !workspace().chat.favorites.is_empty() {
            Some(ConversationSelection::Favorite(0))
        } else {
            self.local_conversations
                .first()
                .map(|_| ConversationSelection::Local(0))
        }
    }

    pub(super) fn handle_local_conversation_action(
        &mut self,
        action: LocalConversationAction,
        toast: &mut ToastState,
    ) {
        match action {
            LocalConversationAction::Pin(id) => {
                match self.set_local_conversation_pinned(id, true) {
                    Ok(true) => toast.show("Local chat pinned to Favorites"),
                    Ok(false) => {}
                    Err(error) => toast.show_error(error.user_message()),
                }
            }
            LocalConversationAction::Unpin(id) => {
                match self.set_local_conversation_pinned(id, false) {
                    Ok(true) => toast.show("Local chat unpinned from Favorites"),
                    Ok(false) => {}
                    Err(error) => toast.show_error(error.user_message()),
                }
            }
            LocalConversationAction::Rename(id) => self.begin_local_conversation_rename(id),
            LocalConversationAction::Remove(id) => self.begin_local_conversation_removal(id),
        }
    }

    pub(super) fn begin_local_conversation_rename(&mut self, id: u64) {
        let Some(index) = self.local_index_for_id(id) else {
            return;
        };
        let draft = self.local_conversations[index].name.clone();
        self.dismiss_contextual_overlays();
        self.editing_local_conversation = Some(LocalConversationEditor {
            id,
            draft,
            focus_requested: true,
        });
    }

    pub(super) fn begin_local_conversation_removal(&mut self, id: u64) {
        let Some(index) = self.local_index_for_id(id) else {
            return;
        };
        let name = self.local_conversations[index].name.clone();
        self.dismiss_contextual_overlays();
        self.removing_local_conversation = Some(LocalConversationRemoval { id, name });
    }

    /// Attachment pickers are transient UI, unlike drafts and pending files.
    /// Close every picker when its owning chat context is no longer on screen so
    /// returning to another conversation never exposes a stale popover.
    pub(super) fn dismiss_attachment_pickers(&mut self) {
        for composer in self
            .favorite_composers
            .iter_mut()
            .chain(self.workspace_composers.iter_mut())
            .chain(
                self.local_conversations
                    .iter_mut()
                    .map(|conversation| &mut conversation.composer),
            )
        {
            composer.attachment_picker_open = false;
        }
    }

    /// Conversations with unread messages, for the rail badge. It counts each
    /// conversation once, covering the catalog chats and the Microsoft 365
    /// chats whose `read_through` marker is behind. A muted remote chat does
    /// not add to the badge, as it does not raise a toast either. Local chats
    /// have no read state.
    pub(crate) fn unread_chat_count(&self) -> usize {
        let catalog = workspace();
        let favorites = (0..catalog.chat.favorites.len())
            .filter(|&index| self.is_unread(ConversationSelection::Favorite(index)))
            .count();
        let workspace_chats = (0..catalog.chat.conversations.len())
            .filter(|&index| self.is_unread(ConversationSelection::Workspace(index)))
            .count();
        let remote = self
            .remote_chats
            .iter()
            .enumerate()
            .filter(|(index, chat)| chat.is_unread() && !self.is_remote_chat_muted(*index))
            .count();
        favorites + workspace_chats + remote
    }

    pub(super) fn is_unread(&self, selection: ConversationSelection) -> bool {
        match selection {
            ConversationSelection::Favorite(index) => {
                workspace()
                    .chat
                    .favorites
                    .get(index)
                    .is_some_and(|chat| chat.unread)
                    && !self.favorite_read.get(index).copied().unwrap_or(false)
            }
            ConversationSelection::Workspace(index) => {
                workspace()
                    .chat
                    .conversations
                    .get(index)
                    .is_some_and(|chat| chat.unread)
                    && !self.workspace_read.get(index).copied().unwrap_or(false)
            }
            ConversationSelection::Remote(index) => self
                .remote_chats
                .get(index)
                .is_some_and(|conversation| conversation.is_unread()),
            ConversationSelection::Local(_) => false,
        }
    }

    fn mark_conversation_read(&mut self, selection: ConversationSelection) {
        if let ConversationSelection::Remote(index) = selection {
            if let Some(conversation) = self.remote_chats.get_mut(index) {
                conversation.mark_read();
            }
            return;
        }
        let read_state = match selection {
            ConversationSelection::Favorite(index) => self.favorite_read.get_mut(index),
            ConversationSelection::Workspace(index) => self.workspace_read.get_mut(index),
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(_) => None,
        };
        let was_marked = read_state.is_some_and(|read| {
            if *read {
                false
            } else {
                *read = true;
                true
            }
        });
        if was_marked {
            self.mark_persistence_dirty();
        }
    }

    pub(super) fn mark_all_conversations_read(&mut self) -> usize {
        let mut locally_persisted_marks = 0;
        for (chat, read) in workspace()
            .chat
            .favorites
            .iter()
            .zip(&mut self.favorite_read)
        {
            if chat.unread && !*read {
                *read = true;
                locally_persisted_marks += 1;
            }
        }
        for (chat, read) in workspace()
            .chat
            .conversations
            .iter()
            .zip(&mut self.workspace_read)
        {
            if chat.unread && !*read {
                *read = true;
                locally_persisted_marks += 1;
            }
        }
        if locally_persisted_marks > 0 {
            self.mark_persistence_dirty();
        }
        locally_persisted_marks + self.remote_chats.mark_all_read()
    }
}
