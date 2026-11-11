use super::ChatState;
use super::persistence::{
    MAX_LOCAL_CONVERSATION_NAME_BYTES, MAX_MESSAGE_TEXT_BYTES, StoredChat, StoredConversation,
};
use crate::data::workspace::workspace;
use crate::shared::model::Message;

use super::{
    ChatQuickView, ConversationSelection, DeletedLocalMessage, LocalConversationMutationError,
    LocalMessageMutationError, local_conversation_initials, message_to_stored,
};

impl ChatState {
    fn stored_conversation_for_selection_mut<'a>(
        &self,
        stored: &'a mut StoredChat,
        selection: ConversationSelection,
    ) -> Option<&'a mut StoredConversation> {
        match selection {
            ConversationSelection::Favorite(index) => {
                let id = &workspace().chat.favorites.get(index)?.id;
                stored.conversations.get_mut(id)
            }
            ConversationSelection::Workspace(index) => {
                let id = &workspace().chat.conversations.get(index)?.id;
                stored.conversations.get_mut(id)
            }
            ConversationSelection::Remote(_) => None,
            ConversationSelection::Local(index) => {
                let local_id = self.local_conversations.get(index)?.id;
                stored
                    .local_conversations
                    .iter_mut()
                    .find(|conversation| conversation.id == local_id)
                    .map(|conversation| &mut conversation.state)
            }
        }
    }

    fn preflight_local_message_mutation(
        &self,
        local_message_id: u64,
        replacement_text: Option<&str>,
    ) -> Result<(), LocalMessageMutationError> {
        let selected = self.selected;
        let mut stored = self.stored();
        let Some(conversation) = self.stored_conversation_for_selection_mut(&mut stored, selected)
        else {
            return Err(LocalMessageMutationError::Missing);
        };
        let Some(index) = conversation
            .messages
            .iter()
            .position(|message| message.local_id == Some(local_message_id) && message.own)
        else {
            return Err(LocalMessageMutationError::Missing);
        };
        if let Some(text) = replacement_text {
            conversation.messages[index].text = text.into();
        } else {
            conversation.messages.remove(index);
        }
        stored
            .validate_for_persistence()
            .map_err(|_| LocalMessageMutationError::NotPersistable)
    }

    pub(super) fn preflight_new_local_message(
        &self,
        selection: ConversationSelection,
        local_message_id: u64,
        next_local_message_id: u64,
        message: &Message,
    ) -> Result<(), LocalMessageMutationError> {
        let mut stored = self.stored();
        stored.next_local_message_id = next_local_message_id;
        let Some(conversation) = self.stored_conversation_for_selection_mut(&mut stored, selection)
        else {
            return Err(LocalMessageMutationError::Missing);
        };
        conversation
            .messages
            .push(message_to_stored(message, Some(local_message_id)));
        conversation.draft.clear();
        conversation.pending_attachment = None;
        stored
            .validate_for_persistence()
            .map_err(|_| LocalMessageMutationError::NotPersistable)
    }

    pub(super) fn edit_local_message(
        &mut self,
        local_message_id: u64,
        draft: &str,
    ) -> Result<(), LocalMessageMutationError> {
        let text = draft.trim();
        if text.is_empty() {
            return Err(LocalMessageMutationError::Empty);
        }
        if text.len() > MAX_MESSAGE_TEXT_BYTES {
            return Err(LocalMessageMutationError::TooLong);
        }
        self.preflight_local_message_mutation(local_message_id, Some(text))?;

        let selected = self.selected;
        let Some(timeline) = self.timeline_for_mut(selected) else {
            return Err(LocalMessageMutationError::Missing);
        };
        let Some(index) = timeline.index_for_local_id(local_message_id) else {
            return Err(LocalMessageMutationError::Missing);
        };
        let Some(message) = timeline
            .messages
            .get_mut(index)
            .filter(|message| message.own)
        else {
            return Err(LocalMessageMutationError::Missing);
        };
        message.text = text.into();
        self.mark_persistence_dirty();
        Ok(())
    }

    pub(super) fn delete_local_message(
        &mut self,
        local_message_id: u64,
    ) -> Result<(), LocalMessageMutationError> {
        self.preflight_local_message_mutation(local_message_id, None)?;

        let selected = self.selected;
        let Some(timeline) = self.timeline_for_mut(selected) else {
            return Err(LocalMessageMutationError::Missing);
        };
        let Some((index, message)) = timeline.remove_locally_authored_at(local_message_id) else {
            return Err(LocalMessageMutationError::Missing);
        };
        if !message.own {
            timeline.insert_locally_authored(index, local_message_id, message);
            return Err(LocalMessageMutationError::Missing);
        }
        if self.shared_preview == message.attachment {
            self.shared_preview = None;
        }
        self.undo_message = Some(DeletedLocalMessage {
            selection: selected,
            index,
            local_id: local_message_id,
            message,
        });
        self.link_preview = None;
        self.editing_message = None;
        self.mark_persistence_dirty();
        Ok(())
    }

    /// Puts back the message the last local delete removed, in its old
    /// position, and persists it again. Fails (leaving the buffer consumed)
    /// when its chat is gone or the restored profile would not fit.
    pub(crate) fn undo_delete_local_message(&mut self) -> bool {
        self.try_undo_delete_local_message().is_ok()
    }

    pub(super) fn try_undo_delete_local_message(
        &mut self,
    ) -> Result<(), LocalMessageMutationError> {
        let Some(deleted) = self.undo_message.take() else {
            return Err(LocalMessageMutationError::Missing);
        };
        let Some(timeline) = self.timeline_for_mut(deleted.selection) else {
            return Err(LocalMessageMutationError::Missing);
        };
        if timeline.index_for_local_id(deleted.local_id).is_some() {
            return Err(LocalMessageMutationError::Missing);
        }
        timeline.insert_locally_authored(deleted.index, deleted.local_id, deleted.message);
        if self.stored().validate_for_persistence().is_err() {
            if let Some(timeline) = self.timeline_for_mut(deleted.selection) {
                let _ = timeline.remove_locally_authored_at(deleted.local_id);
            }
            return Err(LocalMessageMutationError::NotPersistable);
        }
        self.mark_persistence_dirty();
        Ok(())
    }

    /// Builds the exact persisted projection of a local conversation rename or
    /// removal before mutating visible state. This keeps the UI and atomic
    /// snapshot contract aligned even when a profile is close to its size or
    /// structural limits.
    fn preflight_local_conversation_mutation(
        &self,
        id: u64,
        replacement_name: Option<(&str, &str)>,
    ) -> Result<(), LocalConversationMutationError> {
        let mut stored = self.stored();
        let Some(index) = stored
            .local_conversations
            .iter()
            .position(|conversation| conversation.id == id)
        else {
            return Err(LocalConversationMutationError::Missing);
        };

        if let Some((name, initials)) = replacement_name {
            let conversation = &mut stored.local_conversations[index];
            conversation.name = name.into();
            conversation.initials = initials.into();
        } else {
            stored.local_conversations.remove(index);
            // A pin may never outlive its local conversation. Include this
            // cleanup in the prospective snapshot before changing UI state.
            stored
                .pinned_local_conversation_ids
                .retain(|pinned_id| *pinned_id != id);
        }

        stored
            .validate_for_persistence()
            .map_err(|_| LocalConversationMutationError::NotPersistable)
    }

    /// Pins or unpins one locally authored chat after validating the exact
    /// resulting durable projection. The return value reports whether the
    /// state changed; an already-pinned/unpinned chat is a harmless no-op.
    pub(super) fn set_local_conversation_pinned(
        &mut self,
        id: u64,
        pinned: bool,
    ) -> Result<bool, LocalConversationMutationError> {
        if self.local_index_for_id(id).is_none() {
            return Err(LocalConversationMutationError::Missing);
        }
        if self.is_local_conversation_pinned(id) == pinned {
            return Ok(false);
        }

        let mut prospective = self.stored();
        if pinned {
            prospective.pinned_local_conversation_ids.push(id);
        } else {
            prospective
                .pinned_local_conversation_ids
                .retain(|pinned_id| *pinned_id != id);
        }
        prospective
            .validate_for_persistence()
            .map_err(|_| LocalConversationMutationError::NotPersistable)?;

        if pinned {
            self.pinned_local_conversation_ids.push(id);
        } else {
            self.pinned_local_conversation_ids
                .retain(|pinned_id| *pinned_id != id);
        }
        self.mark_persistence_dirty();
        Ok(true)
    }

    pub(super) fn rename_local_conversation(
        &mut self,
        id: u64,
        draft: &str,
    ) -> Result<(), LocalConversationMutationError> {
        let name = draft.trim();
        if name.is_empty() {
            return Err(LocalConversationMutationError::EmptyName);
        }
        if name.len() > MAX_LOCAL_CONVERSATION_NAME_BYTES {
            return Err(LocalConversationMutationError::NameTooLong);
        }
        let initials = local_conversation_initials(name);
        self.preflight_local_conversation_mutation(id, Some((name, &initials)))?;

        // Resolve the durable ID again immediately before changing the vector.
        // A visible local index is only a layout detail and can shift when a
        // different local conversation is removed.
        let Some(index) = self.local_index_for_id(id) else {
            return Err(LocalConversationMutationError::Missing);
        };
        let conversation = &mut self.local_conversations[index];
        if conversation.name != name || conversation.initials != initials {
            conversation.name = name.into();
            conversation.initials = initials;
            self.mark_persistence_dirty();
        }
        Ok(())
    }

    pub(super) fn remove_local_conversation(
        &mut self,
        id: u64,
    ) -> Result<(), LocalConversationMutationError> {
        self.preflight_local_conversation_mutation(id, None)?;

        // Resolve by stable ID at mutation time. Do not retain an index from a
        // menu render or preflight because another local row may have shifted.
        let Some(index) = self.local_index_for_id(id) else {
            return Err(LocalConversationMutationError::Missing);
        };
        let selected_before_removal = self.selected;
        let removed_selected = matches!(selected_before_removal, ConversationSelection::Local(selected_index) if selected_index == index);
        let removed_chat_id = format!("local:{id}");
        // A buffered message may point at a shifted or removed local chat.
        self.undo_message = None;
        self.local_conversations.remove(index);
        self.pinned_local_conversation_ids
            .retain(|pinned_id| *pinned_id != id);

        if let ConversationSelection::Local(selected_index) = selected_before_removal
            && selected_index > index
        {
            self.selected = ConversationSelection::Local(selected_index - 1);
        }

        if self
            .editing_message
            .as_ref()
            .is_some_and(|editor| editor.chat_id == removed_chat_id)
        {
            self.editing_message = None;
        }
        if self
            .editing_local_conversation
            .as_ref()
            .is_some_and(|editor| editor.id == id)
        {
            self.editing_local_conversation = None;
        }
        if self
            .removing_local_conversation
            .as_ref()
            .is_some_and(|removal| removal.id == id)
        {
            self.removing_local_conversation = None;
        }

        // Search results and an active query can retain the just-removed
        // target. Clear them rather than leave a stale selectable result.
        self.query.clear();
        self.chat_search_open = false;
        self.quick_view = ChatQuickView::All;
        self.scroll_to_bottom_requested = false;

        if removed_selected {
            self.shared_preview = None;
            self.link_preview = None;
            self.requested_call = None;
            if let Some(fallback) = self.stable_fallback_selection() {
                self.select_conversation(fallback);
            }
        }

        self.mark_persistence_dirty();
        Ok(())
    }
}
