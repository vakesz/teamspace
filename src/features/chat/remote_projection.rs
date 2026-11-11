use super::ChatState;
use super::RemoteReconciliationResolution;
use super::remote::{
    PresenceSchedule, RemoteChatHistoryState, RemoteSendError, RemoteSubmissionId,
};
use super::{RemoteDeleteConfirmation, RemoteMutationLabel, RemoteMutationResolution};
use crate::features::calls::CallMode;
use crate::integrations::{
    Capability, ProviderResourceRef, RemoteCapabilities, RemoteChatMembersProjection,
    RemoteChatMessagesProjection, RemoteChatProjection, RemotePinnedChatMessagesProjection,
    WorkspaceIntentId,
};
use crate::shared::model::ChatTab;
use crate::shared::reaction::ReactionKind;

use super::{ChatAction, ConversationSelection};

impl ChatState {
    /// Replaces the ephemeral Microsoft 365 chat projection. It never marks
    /// local persistence dirty: a Graph account snapshot must not be written
    /// into the local-preview profile cache.
    pub(crate) fn apply_remote_projection(
        &mut self,
        projection: RemoteChatProjection,
    ) -> Vec<WorkspaceIntentId> {
        let selected_remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        // `replace` may discard an omitted conversation before its transient
        // panel can be closed. Remember the selected panel/request first so
        // the later projection-loss branch still sends its narrowly scoped
        // cancellation signal instead of silently dropping an active poll.
        let selected_pinned_panel_was_open =
            selected_remote_index.is_some_and(|index| self.remote_chats.has_pinned_panel(index));
        let selected_pinned_request_was_pending =
            selected_remote_index.is_some() && self.pending_remote_chat_pinned_messages.is_some();
        let selected_members_panel_was_open =
            selected_remote_index.is_some_and(|index| self.remote_chats.has_members_panel(index));
        let selected_members_request_was_pending =
            selected_remote_index.is_some() && self.pending_remote_chat_members.is_some();
        let (replacement_index, cancel_unstarted) =
            self.remote_chats.replace(projection, selected_remote_index);
        // A projection can remove a conversation while an action is still in
        // this frame's feature queue. Keep remote sends tied to their feature
        // state; reaction actions are checked against the selected, fresh
        // history below after selection and loading state are reconciled.
        let remote_chats = &self.remote_chats;
        self.pending_actions.retain(|action| match action {
            ChatAction::SendRemoteMessage { submission, .. } => {
                remote_chats.has_submission(*submission)
            }
            ChatAction::ReplyToRemoteChatMessageWithQuote { submission, .. } => {
                remote_chats.has_submission(*submission)
            }
            ChatAction::UploadRemoteAttachment { submission, .. }
            | ChatAction::ContinueRemoteUpload { submission, .. } => {
                remote_chats.has_upload_submission(*submission)
            }
            // A cancellation is never lost with its chat.
            ChatAction::CancelRemoteUpload { .. } => true,
            ChatAction::EditRemoteChatMessage { submission, .. }
            | ChatAction::DeleteRemoteChatMessage { submission, .. } => {
                remote_chats.has_mutation_submission(*submission)
            }
            ChatAction::SetRemoteChatMessageReaction { .. } => true,
            ChatAction::SendLocalMessage { .. } => true,
        });
        if selected_remote_index.is_some() {
            if let Some(index) = replacement_index {
                self.selected = ConversationSelection::Remote(index);
                // A directory row is only a preview. Replacing a selected
                // chat's completed history invalidates its reaction controls,
                // so immediately stage a fresh target-bound history read
                // rather than leaving the unchanged selection permanently
                // read-only until the user navigates away and back.
                match self.remote_chats.history_state(index) {
                    Some(RemoteChatHistoryState::NotRequested) => {
                        self.pending_remote_chat_messages =
                            self.remote_chats.begin_history_load(index);
                    }
                    Some(RemoteChatHistoryState::Stale) => {
                        self.pending_remote_chat_messages =
                            self.remote_chats.begin_history_refresh(index);
                    }
                    _ => {}
                }
            } else {
                // A bounded directory response can temporarily omit a chat
                // that still owns a controller-bound non-idempotent reaction.
                // Cancel its selected-history read and discard only an
                // unbound same-frame click; the hidden conversation retains
                // its exact gate until a terminal outcome or later history
                // reconciliation makes another write safe.
                if selected_pinned_panel_was_open || selected_pinned_request_was_pending {
                    self.pending_remote_chat_pinned_messages = None;
                    self.cancel_remote_chat_pinned_messages_requested = true;
                }
                if selected_members_panel_was_open || selected_members_request_was_pending {
                    self.pending_remote_chat_members = None;
                    self.cancel_remote_chat_members_requested = true;
                }
                self.cancel_remote_chat_messages_for_projection_loss();
                self.selected = self
                    .stable_fallback_selection()
                    .unwrap_or(ConversationSelection::Workspace(0));
                self.tab = ChatTab::Chat;
                self.dismiss_contextual_overlays();
            }
        }
        self.retain_dispatchable_remote_reaction_actions();
        cancel_unstarted
    }

    /// Applies one bounded selected-chat history only when its opaque target
    /// still matches the currently visible remote conversation. This is an
    /// ephemeral presentation update: no provider records are persisted and
    /// the existing remote-composer state remains owned by that target.
    pub(crate) fn apply_remote_chat_messages_projection(
        &mut self,
        projection: RemoteChatMessagesProjection,
    ) -> bool {
        let selected_remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        let applied = self
            .remote_chats
            .apply_selected_history(selected_remote_index, projection);
        if applied {
            // A selected-history replacement can remove, delete, or already
            // satisfy a click that has not reached the controller. Prune that
            // feature action before the app drains it; bound tombstones live
            // solely in `RemoteChatState` and are deliberately untouched.
            self.retain_dispatchable_remote_reaction_actions();
        }
        applied
    }

    /// Drains the one-shot selected-chat read target. The app hands the opaque
    /// value directly to the controller, which alone can choose a provider
    /// route. Repeated UI frames cannot replay it after this drain.
    pub(crate) fn take_remote_chat_messages_request(&mut self) -> Option<ProviderResourceRef> {
        self.pending_remote_chat_messages.take()
    }

    /// Drains the links the user clicked in formatted remote messages.
    pub(crate) fn take_remote_link_opens(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_link_opens)
    }

    /// Drains a selection-scoped cancellation signal. This can be true even
    /// after the request target was already drained, because its worker may
    /// still be active when the user leaves the remote chat.
    pub(crate) fn take_remote_chat_messages_cancellation_requested(&mut self) -> bool {
        std::mem::take(&mut self.cancel_remote_chat_messages_requested)
    }

    /// Applies a selected remote chat's bounded pinned-message projection
    /// only while that exact chat remains selected and its explicit panel is
    /// open. The panel is session-only, so a late result never becomes a
    /// local timeline, preference, or persisted record.
    pub(crate) fn apply_remote_chat_pinned_messages_projection(
        &mut self,
        projection: RemotePinnedChatMessagesProjection,
    ) -> bool {
        let selected_remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        self.remote_chats
            .apply_selected_pinned_messages(selected_remote_index, projection)
    }

    /// Drains one explicit selected-chat pinned-message request. The opaque
    /// target crosses directly to the app-owned transport controller; feature
    /// code never sees a Graph route, pin identifier, or raw response.
    pub(crate) fn take_remote_chat_pinned_messages_request(
        &mut self,
    ) -> Option<ProviderResourceRef> {
        self.pending_remote_chat_pinned_messages.take()
    }

    /// Drains only the contextual pinned-message cancellation signal. It is
    /// independent from selected message history, global chat refreshes, and
    /// non-idempotent delivery reconciliation.
    pub(crate) fn take_remote_chat_pinned_messages_cancellation_requested(&mut self) -> bool {
        std::mem::take(&mut self.cancel_remote_chat_pinned_messages_requested)
    }

    /// Applies a selected remote chat's bounded People roster only while that
    /// exact chat remains selected and its explicit panel is open.
    pub(crate) fn apply_remote_chat_members_projection(
        &mut self,
        projection: RemoteChatMembersProjection,
    ) -> bool {
        let selected_remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        self.remote_chats
            .apply_selected_members(selected_remote_index, projection)
    }

    /// Drains one explicit selected-chat People request. The opaque target
    /// crosses directly to the app-owned transport controller.
    pub(crate) fn take_remote_chat_members_request(&mut self) -> Option<ProviderResourceRef> {
        self.pending_remote_chat_members.take()
    }

    /// The People presence schedule for the selected chat. Refresh runs only
    /// while the panel is open and the window is focused, at most every 60 s.
    pub(crate) fn remote_chat_member_presence_schedule(
        &mut self,
        now: std::time::Instant,
        focused: bool,
    ) -> PresenceSchedule {
        match self.selected {
            ConversationSelection::Remote(index) => {
                self.remote_chats.presence_schedule(index, now, focused)
            }
            _ => PresenceSchedule::Idle,
        }
    }

    /// The one window-focus signal from the app shell. A new message in the
    /// selected chat is acknowledged as read only while focused.
    pub(crate) fn set_window_focused(&mut self, focused: bool) {
        self.remote_chats.set_window_focused(focused);
    }

    /// The window regained focus: the open remote chat is in view again, so
    /// what arrived while away is now seen.
    pub(crate) fn on_window_focus_gained(&mut self) {
        if let ConversationSelection::Remote(index) = self.selected {
            self.remote_chats.mark_read_at(index);
        }
    }

    /// Starts a refresh of the selected remote chat's loaded history for an
    /// explicit event. The loaded rows stay visible but non-actionable until
    /// the exact read replaces them. Only a completed or stale history
    /// refreshes; a loading, denied or unselected chat returns `None`.
    pub(crate) fn begin_selected_remote_history_refresh(&mut self) -> Option<ProviderResourceRef> {
        match self.selected {
            ConversationSelection::Remote(index) => self.remote_chats.begin_history_refresh(index),
            _ => None,
        }
    }

    /// Drains only the contextual People cancellation signal.
    pub(crate) fn take_remote_chat_members_cancellation_requested(&mut self) -> bool {
        std::mem::take(&mut self.cancel_remote_chat_members_requested)
    }

    /// Clears remote account data on disconnect, sign-out, or account change.
    /// Local preview conversations and their stored overlays are retained.
    pub(crate) fn clear_remote_projection(&mut self) {
        let remote_was_selected = matches!(self.selected, ConversationSelection::Remote(_));
        if remote_was_selected || self.pending_remote_chat_messages.is_some() {
            self.cancel_remote_chat_messages();
        }
        if remote_was_selected
            || self.pending_remote_chat_pinned_messages.is_some()
            || self.remote_chats.has_open_pinned_messages()
        {
            self.cancel_remote_chat_pinned_messages_for_projection_loss();
        }
        if remote_was_selected
            || self.pending_remote_chat_members.is_some()
            || self.remote_chats.has_open_members()
        {
            self.cancel_remote_chat_members_for_projection_loss();
        }
        self.remote_chats.clear();
        self.reset_remote_capabilities();
        self.pending_actions.retain(|action| {
            !matches!(
                action,
                ChatAction::SendRemoteMessage { .. }
                    | ChatAction::UploadRemoteAttachment { .. }
                    | ChatAction::ContinueRemoteUpload { .. }
                    | ChatAction::ReplyToRemoteChatMessageWithQuote { .. }
                    | ChatAction::SetRemoteChatMessageReaction { .. }
                    | ChatAction::EditRemoteChatMessage { .. }
                    | ChatAction::DeleteRemoteChatMessage { .. }
            )
        });
        if remote_was_selected {
            self.selected = self
                .stable_fallback_selection()
                .unwrap_or(ConversationSelection::Workspace(0));
            self.tab = ChatTab::Chat;
            self.dismiss_contextual_overlays();
        }
    }

    /// Clears visible Microsoft 365 chat data after a transport-worker loss
    /// without pretending that an already-dispatched reaction did not reach
    /// the provider.
    ///
    /// Unlike [`Self::clear_remote_projection`], this is not an explicit
    /// account/disconnect boundary. It keeps only controller-bound reaction
    /// tombstones so a later delivery-uncertain or terminal outcome can still
    /// be routed safely. No visible rows, remote drafts, sends, or unbound
    /// feature actions survive the loss.
    pub(crate) fn clear_remote_projection_preserving_delivery_recovery(&mut self) {
        let remote_was_selected = matches!(self.selected, ConversationSelection::Remote(_));
        let had_history_request = self.pending_remote_chat_messages.take().is_some();
        if remote_was_selected || had_history_request {
            self.cancel_remote_chat_messages_requested = true;
        }
        self.cancel_remote_chat_pinned_messages_for_projection_loss();
        self.cancel_remote_chat_members_for_projection_loss();
        self.remote_chats
            .clear_projection_preserving_bound_reactions();
        self.reset_remote_capabilities();
        self.pending_actions.retain(|action| {
            !matches!(
                action,
                ChatAction::SendRemoteMessage { .. }
                    | ChatAction::UploadRemoteAttachment { .. }
                    | ChatAction::ContinueRemoteUpload { .. }
                    | ChatAction::ReplyToRemoteChatMessageWithQuote { .. }
                    | ChatAction::SetRemoteChatMessageReaction { .. }
                    | ChatAction::EditRemoteChatMessage { .. }
                    | ChatAction::DeleteRemoteChatMessage { .. }
            )
        });
        if remote_was_selected {
            self.selected = self
                .stable_fallback_selection()
                .unwrap_or(ConversationSelection::Workspace(0));
            self.tab = ChatTab::Chat;
            self.dismiss_contextual_overlays();
        }
    }

    pub(crate) fn take_requested_call(&mut self) -> Option<(String, CallMode)> {
        self.requested_call.take()
    }

    /// Drains accepted user intents without persisting or replaying them.
    ///
    /// The app owns transport dispatch, so this feature deliberately exposes
    /// domain actions rather than a provider client or transport command.
    pub(crate) fn take_actions(&mut self) -> Vec<ChatAction> {
        // This is the final feature-to-app boundary. Recheck reaction and
        // quote actions here as well as after projections because a same-chat
        // selection can start a new history read after a click was staged.
        self.retain_dispatchable_remote_reaction_actions();
        std::mem::take(&mut self.pending_actions)
    }

    /// Retains only a click whose exact route still belongs to the currently
    /// selected, ready history and whose provider-owned selection has not
    /// already reached the requested state. This prevents an unbound feature
    /// action from becoming a late Graph write after any UI or projection
    /// transition; controller-bound tombstones are not actions and remain in
    /// the remote state for outcome routing.
    pub(super) fn retain_dispatchable_remote_reaction_actions(&mut self) {
        let selected_remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        let remote_chats = &self.remote_chats;
        let mut pruned_reactions = Vec::new();
        let mut pruned_mutations = Vec::new();
        self.pending_actions.retain(|action| match action {
            ChatAction::EditRemoteChatMessage {
                submission,
                chat,
                message,
                ..
            }
            | ChatAction::DeleteRemoteChatMessage {
                submission,
                chat,
                message,
            } => {
                let dispatchable = remote_chats.has_dispatchable_mutation(
                    selected_remote_index,
                    *submission,
                    chat,
                    message,
                );
                if !dispatchable {
                    pruned_mutations.push(*submission);
                }
                dispatchable
            }
            ChatAction::ReplyToRemoteChatMessageWithQuote {
                submission,
                chat,
                messages,
                ..
            } => remote_chats.has_dispatchable_quote_submission(
                selected_remote_index,
                *submission,
                chat,
                messages,
            ),
            ChatAction::SetRemoteChatMessageReaction {
                chat,
                message,
                reaction,
                selected,
            } => {
                let dispatchable = remote_chats.has_dispatchable_reaction(
                    selected_remote_index,
                    chat,
                    message,
                    *reaction,
                    *selected,
                );
                if !dispatchable {
                    pruned_reactions.push((chat.clone(), message.clone(), *reaction, *selected));
                }
                dispatchable
            }
            ChatAction::SendLocalMessage { .. }
            | ChatAction::SendRemoteMessage { .. }
            | ChatAction::UploadRemoteAttachment { .. }
            | ChatAction::ContinueRemoteUpload { .. }
            | ChatAction::CancelRemoteUpload { .. } => true,
        });
        // A pruned click can never reach the controller, so its unbound gate
        // would otherwise lock that reaction until the conversation is
        // replaced. Controller-bound gates are untouched here.
        for (chat, message, reaction, selected) in pruned_reactions {
            self.remote_chats
                .discard_unbound_reaction(&chat, &message, reaction, selected);
        }
        // A pruned edit or deletion can never reach the controller, so give
        // the user their text back and unlock the conversation.
        for submission in pruned_mutations {
            self.remote_chats
                .restore_unbound_mutation_for_submission(submission);
        }
    }

    /// Correlates a feature-local remote submission with the controller-owned
    /// intent allocated when it entered the transport FIFO. The intent is
    /// opaque to the feature; it only lets later outcomes update the correct
    /// session-only composer state.
    pub(crate) fn bind_remote_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote_chats.bind_submission(submission, intent_id)
            || self
                .remote_chats
                .bind_mutation_submission(submission, intent_id)
    }

    /// Correlates an exact provider-owned chat/message reaction gate with the
    /// controller intent allocated for it. The chat target participates in the
    /// lookup so a same-shaped message target in another conversation cannot
    /// receive this outcome.
    pub(crate) fn bind_remote_chat_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.remote_chats
            .bind_reaction(intent_id, chat, message, reaction)
    }

    /// Whether this exact chat/message reaction intent is still owned by the
    /// feature. A confirmed delivery, a provider throttle and an ambiguous write
    /// all leave the same gate locked until the exact selected history reports
    /// the requested state, so one predicate serves all three outcomes.
    pub(crate) fn remote_chat_reaction_owned(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.owns_reaction_intent_anywhere(intent_id)
    }

    /// Only a known rejection unlocks the exact chat/message reaction gate.
    pub(crate) fn remote_chat_reaction_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats
            .resolve_reaction_terminal_failure(intent_id)
    }

    /// Clears a pending remote composer after a transport receipt. The
    /// visible timeline remains provider-owned, so no local message is
    /// appended here; the controller schedules a fresh remote projection.
    pub(crate) fn remote_send_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.resolve_delivered(intent_id)
    }

    /// A provider-confirmed throttle retains the locked draft. The controller
    /// owns the known-safe retry timing, while the feature prevents duplicate
    /// user submissions until it receives a terminal outcome.
    pub(crate) fn remote_send_retry_scheduled(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.resolve_retry_scheduled(intent_id)
    }

    /// Restores the remote draft after a provider-confirmed rejection. An
    /// ambiguous POST uses the separate reconciliation transition below so it
    /// cannot be resent before a fresh bounded provider snapshot arrives.
    pub(crate) fn remote_send_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.resolve_terminal_failure(intent_id)
    }

    /// Locks an ambiguous remote send until the controller completes its
    /// specifically scheduled Chats reconciliation. The original body remains
    /// session-only and is never emitted as a second command automatically.
    pub(crate) fn remote_send_delivery_uncertain(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.resolve_delivery_uncertain(intent_id)
    }

    pub(crate) fn complete_remote_send_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteReconciliationResolution> {
        self.remote_chats.complete_reconciliation(intent_id)
    }

    pub(crate) fn fail_remote_send_reconciliation(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.fail_reconciliation(intent_id)
    }

    /// Records the capabilities reported with the identity poll. A `403`
    /// already seen this session keeps its action `NotGranted`.
    pub(crate) fn apply_remote_capabilities(&mut self, capabilities: RemoteCapabilities) {
        self.remote_capabilities = capabilities;
    }

    fn reset_remote_capabilities(&mut self) {
        self.remote_capabilities = RemoteCapabilities::default();
        self.remote_edit_forbidden = false;
        self.remote_delete_forbidden = false;
        self.remote_mark_read_forbidden = false;
        self.remote_file_attach_forbidden = false;
        self.remote_account_id = None;
        self.read_receipt_sent = None;
        self.confirming_remote_delete = None;
    }

    /// The reported capabilities with any session `403` applied.
    pub(crate) fn effective_remote_capabilities(&self) -> RemoteCapabilities {
        let mut capabilities = self.remote_capabilities;
        if self.remote_edit_forbidden {
            capabilities.chat_message_edit = Capability::NotGranted;
        }
        if self.remote_delete_forbidden {
            capabilities.chat_message_delete = Capability::NotGranted;
        }
        capabilities.chat_mark_read =
            self.effective_mark_read_capability(capabilities.chat_mark_read);
        if self.remote_file_attach_forbidden {
            capabilities.file_attach = Capability::NotGranted;
        }
        capabilities
    }

    pub(super) fn begin_remote_edit(
        &mut self,
        remote_index: usize,
        message_index: usize,
    ) -> Result<(), RemoteSendError> {
        let capabilities = self.effective_remote_capabilities();
        self.remote_chats
            .begin_edit(remote_index, message_index, capabilities)
    }

    /// Opens the delete confirmation for one row of the selected, Ready
    /// history. Nothing is sent until the user confirms.
    pub(super) fn request_remote_delete(
        &mut self,
        remote_index: usize,
        message_index: usize,
    ) -> Result<(), RemoteSendError> {
        let capabilities = self.effective_remote_capabilities();
        let view = self
            .remote_chats
            .message_actions_view(remote_index, message_index, capabilities)
            .filter(|view| view.show_delete)
            .ok_or(RemoteSendError::DeleteUnavailable)?;
        debug_assert!(view.show_delete);
        let (chat, message, preview) = self
            .remote_chats
            .delete_candidate(remote_index, message_index)
            .ok_or(RemoteSendError::DeleteUnavailable)?;
        self.confirming_remote_delete = Some(RemoteDeleteConfirmation {
            chat,
            message,
            preview,
        });
        Ok(())
    }

    /// Turns a confirmed deletion into one feature action, after rechecking
    /// that the exact row is still a deletable own message.
    pub(super) fn confirm_remote_delete(&mut self) -> Result<(), RemoteSendError> {
        let confirmation = self
            .confirming_remote_delete
            .take()
            .ok_or(RemoteSendError::DeleteUnavailable)?;
        let capabilities = self.effective_remote_capabilities();
        let index = self
            .remote_chats
            .index_of(&confirmation.chat)
            .ok_or(RemoteSendError::Missing)?;
        if !self
            .remote_chats
            .can_still_delete(index, &confirmation.message, capabilities)
        {
            return Err(RemoteSendError::DeleteUnavailable);
        }
        if let super::remote::RemoteSendRequest::Delete {
            submission,
            target,
            message,
        } = self
            .remote_chats
            .queue_delete(index, &confirmation.message, capabilities)?
        {
            self.pending_actions
                .push(ChatAction::DeleteRemoteChatMessage {
                    submission,
                    chat: target,
                    message,
                });
        }
        Ok(())
    }

    /// `204`: the exact gate stays until the refreshed history arrives.
    pub(crate) fn remote_message_mutation_delivered(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote_chats.mutation_delivered(intent_id)
    }

    /// Whether this intent belongs to an edit or deletion (used for a
    /// provider throttle, where the gate must simply stay locked).
    pub(crate) fn remote_message_mutation_owned(&self, intent_id: WorkspaceIntentId) -> bool {
        self.remote_chats.mutation_owned(intent_id)
    }

    /// A known rejection unlocks only the exact gate. `forbidden` is a `403`:
    /// the action becomes `NotGranted` for the session.
    pub(crate) fn remote_message_mutation_rejected(
        &mut self,
        intent_id: WorkspaceIntentId,
        forbidden: bool,
    ) -> Option<RemoteMutationLabel> {
        let label = self.remote_chats.mutation_rejected(intent_id)?;
        if forbidden {
            match label {
                RemoteMutationLabel::Edit => self.remote_edit_forbidden = true,
                RemoteMutationLabel::Delete => self.remote_delete_forbidden = true,
            }
        }
        Some(label)
    }

    pub(crate) fn remote_message_mutation_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote_chats.mutation_uncertain(intent_id)
    }

    pub(crate) fn complete_remote_message_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationResolution> {
        self.remote_chats
            .complete_mutation_reconciliation(intent_id)
    }

    pub(crate) fn fail_remote_message_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationLabel> {
        self.remote_chats.fail_mutation_reconciliation(intent_id)
    }
}
