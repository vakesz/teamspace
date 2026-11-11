//! Ephemeral presentation projection for Microsoft 365 chat data.
//!
//! The integration layer supplies a normalized snapshot with opaque provider
//! targets. This module adapts that snapshot to the Chat UI without adding
//! remote records to the local-preview persistence projection. Remote sending
//! and bounded typed reactions have separate, explicit reconciliation
//! contracts; attachments, edits, and deletes remain unavailable until their
//! provider IDs and delivery semantics are modeled independently.

use crate::integrations::{
    ComposedBody, DraftMention, MentionHandle, ProviderResourceRef, RemoteChatConversation,
    RemoteChatKind, RemoteChatMembersProjection, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, RemoteChatProjection, RemoteMemberPresence, RemoteMessageInstant,
    RemotePinnedChatMessagesProjection, UploadedFileHandle, WorkspaceIntentId,
};
use crate::shared::local_time;
use crate::shared::model::{Message, MessageReply};
use crate::shared::reaction::ReactionKind;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::list_row::ListRow;
use crate::ui::theme;
use eframe::egui::{self, Color32};

mod arrivals;
mod attachment;
mod mutation;

pub(super) use self::arrivals::NewActivityKind;
use self::arrivals::PendingArrival;
use self::attachment::ComposerAttachment;
pub(crate) use self::attachment::RemoteUploadOutcome;
pub(in crate::features::chat) use self::attachment::{AttachmentView, UploadFollowUp};
pub(in crate::features::chat) use self::mutation::RemoteMessageActionsView;
use self::mutation::{PendingRemoteMessageMutation, RemoteEditSelection, same_visible_text};
pub(crate) use self::mutation::{RemoteMutationLabel, RemoteMutationResolution};

/// Upper bound for chats retained only for an unsent draft after they leave
/// the bounded directory projection.
const MAX_HIDDEN_REMOTE_CONVERSATIONS: usize = 50;

#[derive(Default)]
pub(super) struct RemoteChatState {
    conversations: Vec<RemoteConversation>,
    /// Session-only state for a projected target that fell outside a later
    /// bounded Graph snapshot while it still owns an accepted send or a
    /// bound reaction. The app asks the controller to cancel only unstarted
    /// intents; an in-flight outcome can still resolve this tombstone without
    /// losing its draft or reaction gate.
    hidden_conversations: Vec<RemoteConversation>,
    next_submission_id: u64,
    /// The app window is unfocused. Inverted so the default (and a headless
    /// test) is focused. While set, a new arrival in the selected chat is not
    /// acknowledged as read.
    window_unfocused: bool,
    /// New-message detection (see `arrivals`): the first projection of a
    /// session seeds silently, then arrivals queue until drained.
    seeded: bool,
    arrival_floor: Option<RemoteMessageInstant>,
    pending_arrivals: Vec<PendingArrival>,
}

impl RemoteChatState {
    pub(super) fn set_window_focused(&mut self, focused: bool) {
        self.window_unfocused = !focused;
    }

    /// Replaces the whole provider snapshot atomically. Remote records are
    /// session-only and intentionally do not merge with fixture or local
    /// conversations, so an account switch cannot leave stale remote data in
    /// a local profile snapshot.
    /// Returns the replacement index for an already-selected remote chat when
    /// its opaque provider target is still present. This preserves selection
    /// across a normal reorder without ever matching by a display label.
    pub(super) fn replace(
        &mut self,
        projection: RemoteChatProjection,
        selected_index: Option<usize>,
    ) -> (Option<usize>, Vec<WorkspaceIntentId>) {
        let selected_target = selected_index
            .and_then(|index| self.conversations.get(index))
            .map(|conversation| conversation.target.clone());
        let mut previous = std::mem::take(&mut self.conversations);
        let mut hidden = std::mem::take(&mut self.hidden_conversations);
        self.conversations = projection
            .conversations()
            .iter()
            .map(|conversation| {
                let mut replacement = RemoteConversation::from_projection(conversation);
                if let Some(previous) =
                    take_conversation_for_target(&mut previous, &replacement.target)
                        .or_else(|| take_conversation_for_target(&mut hidden, &replacement.target))
                {
                    // Drafts and in-flight sends belong to the opaque remote
                    // target, not an unstable row index or display name.
                    let keep_loaded_history = selected_target.as_ref() == Some(&replacement.target);
                    replacement.inherit_session_state(
                        previous,
                        keep_loaded_history,
                        keep_loaded_history && !self.window_unfocused,
                    );
                }
                // A directory replacement is never delivery proof: its preview
                // row carries neither priority nor a quote reference. Only an
                // exact selected-history read can confirm an ambiguous send.
                // Unbound quote actions are restored before a stale row could
                // cross the feature-to-app boundary.
                replacement.discard_unbound_quote_reply_when_history_unavailable();
                replacement
            })
            .collect();
        self.observe_arrivals(selected_target.as_ref(), !self.window_unfocused);
        let mut cancel_unstarted = Vec::new();
        for mut conversation in previous.into_iter().chain(hidden) {
            // An action that has not reached the app/controller yet must not
            // survive an absent target and later POST to a hidden chat.
            conversation.restore_unbound_pending_to_draft();
            if let Some(intent_id) = conversation.request_unstarted_cancellation() {
                cancel_unstarted.push(intent_id);
            }
            if conversation.retains_session_state() {
                self.hidden_conversations.push(conversation);
            }
        }
        self.evict_excess_hidden_drafts();
        let replacement_index = selected_target.and_then(|target| {
            self.conversations
                .iter()
                .position(|conversation| conversation.target == target)
        });
        (replacement_index, cancel_unstarted)
    }

    /// Bounds the tombstones kept for chats that fell out of the directory.
    /// The list is newest-first, so the oldest are at the tail. Only a plain
    /// unsent draft is evictable: a bound send, a reconciliation, or a reaction
    /// gate must stay until its outcome arrives. If every excess entry is
    /// gated, the list may exceed the bound rather than lose an outcome.
    fn evict_excess_hidden_drafts(&mut self) {
        while self.hidden_conversations.len() > MAX_HIDDEN_REMOTE_CONVERSATIONS {
            let Some(index) = self
                .hidden_conversations
                .iter()
                .rposition(RemoteConversation::retains_only_a_draft)
            else {
                break;
            };
            self.hidden_conversations.remove(index);
        }
    }

    #[cfg(test)]
    pub(super) fn hidden_count(&self) -> usize {
        self.hidden_conversations.len()
    }

    pub(super) fn clear(&mut self) {
        self.conversations.clear();
        self.hidden_conversations.clear();
        self.reset_arrivals();
    }

    /// Drops every visible remote projection after a transport/worker loss
    /// while retaining only controller-bound, non-idempotent reaction gates.
    ///
    /// This is deliberately narrower than [`Self::clear`], which is used for
    /// an explicit account or disconnect boundary. A lost worker cannot prove
    /// whether an already-dispatched reaction reached Microsoft 365, so its
    /// target-bound tombstone must survive long enough for the controller to
    /// classify a later uncertain or terminal outcome. Remote drafts, sends,
    /// unbound clicks, and presentation rows are not retained.
    pub(super) fn clear_projection_preserving_bound_reactions(&mut self) {
        self.reset_arrivals();
        let conversations = std::mem::take(&mut self.conversations);
        let hidden = std::mem::take(&mut self.hidden_conversations);
        self.hidden_conversations = conversations
            .into_iter()
            .chain(hidden)
            .filter_map(|mut conversation| {
                conversation
                    .retain_bound_reaction_tombstone()
                    .then_some(conversation)
            })
            .collect();
    }

    #[cfg(test)]
    pub(super) const fn is_empty(&self) -> bool {
        self.conversations.is_empty()
    }

    pub(super) fn get(&self, index: usize) -> Option<&RemoteConversation> {
        self.conversations.get(index)
    }

    pub(super) fn get_mut(&mut self, index: usize) -> Option<&mut RemoteConversation> {
        self.conversations.get_mut(index)
    }

    /// Starts a selected-chat history read without exposing its provider route
    /// to the feature. The returned target remains opaque and is handed back
    /// to the app's transport lifecycle as-is.
    /// Acknowledges the newest message of the selected chat, for a window
    /// that just regained focus while that chat was open.
    /// The chat's target when a read receipt could be sent for it: its history
    /// is `Ready` from an exact read and the provider still reports it unread.
    pub(super) fn read_receipt_candidate(&self, index: usize) -> Option<ProviderResourceRef> {
        self.conversations.get(index).and_then(|conversation| {
            (conversation.provider_unread
                && matches!(conversation.history_state, RemoteChatHistoryState::Ready))
            .then(|| conversation.target.clone())
        })
    }

    /// Opaque target and badge count of one directory row, for the mute
    /// digest and the sidebar.
    pub(super) fn target_at(&self, index: usize) -> Option<&ProviderResourceRef> {
        self.conversations
            .get(index)
            .map(|conversation| &conversation.target)
    }

    pub(super) fn title_at(&self, index: usize) -> Option<&str> {
        self.conversations
            .get(index)
            .map(|conversation| conversation.title.as_str())
    }

    pub(super) fn mark_read_at(&mut self, index: usize) {
        if let Some(conversation) = self.conversations.get_mut(index) {
            conversation.mark_read();
        }
    }

    pub(super) fn begin_history_refresh(&mut self, index: usize) -> Option<ProviderResourceRef> {
        self.conversations
            .get_mut(index)
            .and_then(RemoteConversation::begin_history_refresh)
    }

    pub(super) fn begin_history_load(&mut self, index: usize) -> Option<ProviderResourceRef> {
        self.conversations
            .get_mut(index)
            .map(RemoteConversation::begin_history_load)
    }

    /// Applies a normalized selected-history result only when it still belongs
    /// to the chat that is visibly selected. A late worker result for a
    /// previous sidebar selection cannot overwrite the current conversation.
    pub(super) fn apply_selected_history(
        &mut self,
        selected_index: Option<usize>,
        projection: RemoteChatMessagesProjection,
    ) -> bool {
        let selected_conversation = selected_index
            .and_then(|index| self.conversations.get_mut(index))
            .filter(|conversation| conversation.target == projection.chat());
        if let Some(conversation) = selected_conversation {
            conversation.apply_history(projection);
            return true;
        }
        // The exact history read of an ambiguous send's chat can finish after
        // the user navigated elsewhere. It may settle that chat's delivery
        // reconciliation, but it must not replace rows or any other state of
        // a conversation that is not selected.
        let chat = projection.chat();
        if let Some(conversation) = self
            .conversations
            .iter_mut()
            .find(|conversation| conversation.target == chat)
        {
            conversation.observe_reconciliation_history(&projection);
        }
        false
    }

    /// Opens a session-only, target-bound pinned-message panel for the
    /// current remote chat. The caller receives only the opaque chat target
    /// for the app-owned transport seam; no row target or Graph route crosses
    /// into UI state.
    pub(super) fn open_pinned_messages(&mut self, index: usize) -> Option<ProviderResourceRef> {
        self.conversations
            .get_mut(index)
            .and_then(RemoteConversation::open_pinned_messages)
    }

    /// Closes one visible pinned-message panel and drops its safe rows. A
    /// `true` result means an active or queued target-bound read must be
    /// cancelled by the app-owned transport controller.
    pub(super) fn close_pinned_messages(&mut self, index: usize) -> bool {
        self.conversations
            .get_mut(index)
            .is_some_and(RemoteConversation::close_pinned_messages)
    }

    /// Clears every transient pinned-message panel before a projection/account
    /// boundary. These rows are read-only UI state, never a delivery
    /// tombstone, so they must not survive a lost remote projection.
    pub(super) fn close_all_pinned_messages(&mut self) -> bool {
        let mut closed_any = false;
        for conversation in self
            .conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
        {
            // Do not short-circuit: each conversation owns independent
            // session-only panel state that must be cleared at this boundary.
            closed_any |= conversation.close_pinned_messages();
        }
        closed_any
    }

    pub(super) fn has_open_pinned_messages(&self) -> bool {
        self.conversations
            .iter()
            .chain(&self.hidden_conversations)
            .any(RemoteConversation::has_open_pinned_messages)
    }

    /// Applies a bounded pinned-message projection only while its chat is
    /// still visibly selected and its contextual panel remains open. A late
    /// response for a previous chat or a closed panel is deliberately ignored.
    pub(super) fn apply_selected_pinned_messages(
        &mut self,
        selected_index: Option<usize>,
        projection: RemotePinnedChatMessagesProjection,
    ) -> bool {
        let Some(selected_index) = selected_index else {
            return false;
        };
        let Some(conversation) = self.conversations.get_mut(selected_index) else {
            return false;
        };
        if conversation.target != projection.chat() || !conversation.has_open_pinned_messages() {
            return false;
        }
        conversation.apply_pinned_messages(projection);
        true
    }

    /// Returns a cloned, presentation-only panel model. It intentionally has
    /// no provider target or mutation capability, so the compact UI cannot
    /// turn a pinned row into a quote, reaction, edit, or delete action.
    pub(super) fn pinned_messages_panel(
        &self,
        index: usize,
    ) -> Option<RemotePinnedMessagesPanel<'_>> {
        self.conversations
            .get(index)
            .and_then(RemoteConversation::pinned_messages_panel)
    }

    /// Cheap presence check for the selected conversation's pinned panel; it
    /// avoids building a panel view just to ask whether one is open.
    pub(super) fn has_pinned_panel(&self, index: usize) -> bool {
        self.conversations
            .get(index)
            .is_some_and(RemoteConversation::has_open_pinned_messages)
    }

    /// Opens a session-only, target-bound People panel for the current remote
    /// chat. The opaque chat target crosses only to the app-owned transport
    /// seam; member identifiers and Graph route details remain unavailable to
    /// this feature.
    pub(super) fn open_members(&mut self, index: usize) -> Option<ProviderResourceRef> {
        self.conversations
            .get_mut(index)
            .and_then(RemoteConversation::open_members)
    }

    /// Closes one visible People panel and drops its safe rows. A `true`
    /// result means an active or queued target-bound read must be cancelled by
    /// the app-owned transport controller.
    pub(super) fn close_members(&mut self, index: usize) -> bool {
        self.conversations
            .get_mut(index)
            .is_some_and(RemoteConversation::close_members)
    }

    /// Clears every transient People panel before a projection/account
    /// boundary. Roster rows are read-only session state, never a delivery
    /// tombstone, so no hidden conversation may retain them.
    pub(super) fn close_all_members(&mut self) -> bool {
        let mut closed_any = false;
        for conversation in self
            .conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
        {
            // Do not short-circuit: every conversation may own independent
            // session-only People panel state that must be cleared here.
            closed_any |= conversation.close_members();
        }
        closed_any
    }

    /// The presence schedule of the selected chat's People panel.
    pub(super) fn presence_schedule(
        &mut self,
        index: usize,
        now: std::time::Instant,
        focused: bool,
    ) -> PresenceSchedule {
        self.conversations
            .get_mut(index)
            .map_or(PresenceSchedule::Idle, |conversation| {
                conversation.presence_schedule(now, focused)
            })
    }

    pub(super) fn has_open_members(&self) -> bool {
        self.conversations
            .iter()
            .chain(&self.hidden_conversations)
            .any(RemoteConversation::has_open_members)
    }

    /// Applies a bounded member projection only while its chat is still
    /// visibly selected and its explicit People panel remains open. A late
    /// result for a previous chat or closed panel is deliberately ignored.
    pub(super) fn apply_selected_members(
        &mut self,
        selected_index: Option<usize>,
        projection: RemoteChatMembersProjection,
    ) -> bool {
        let Some(selected_index) = selected_index else {
            return false;
        };
        let Some(conversation) = self.conversations.get_mut(selected_index) else {
            return false;
        };
        if conversation.target != projection.chat() || !conversation.has_open_members() {
            return false;
        }
        conversation.apply_members(projection);
        true
    }

    /// Returns a cloned, presentation-only People panel model. It contains no
    /// member/user/provider identifiers or management capability, so UI code
    /// cannot turn roster rows into a mutation route.
    pub(super) fn members_panel(&self, index: usize) -> Option<RemoteChatMembersPanel<'_>> {
        self.conversations
            .get(index)
            .and_then(RemoteConversation::members_panel)
    }

    /// Cheap presence check for the selected conversation's People panel.
    pub(super) fn has_members_panel(&self, index: usize) -> bool {
        self.conversations
            .get(index)
            .is_some_and(RemoteConversation::has_open_members)
    }

    pub(super) fn history_state(&self, index: usize) -> Option<RemoteChatHistoryState> {
        self.conversations
            .get(index)
            .map(RemoteConversation::history_state)
    }

    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = &RemoteConversation> {
        self.conversations.iter()
    }

    /// Marks only the currently projected remote rows as read in this
    /// session. This never sends a Graph read receipt or changes local
    /// preview persistence.
    pub(super) fn mark_all_read(&mut self) -> usize {
        let mut marked = 0;
        for conversation in &mut self.conversations {
            marked += usize::from(conversation.mark_read_if_unread());
        }
        marked
    }

    pub(super) fn queue_message(
        &mut self,
        index: usize,
        capabilities: crate::integrations::RemoteCapabilities,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.conversations.get(index).is_none() {
            return Err(RemoteSendError::Missing);
        }
        if self.conversations[index].is_editing() {
            return self.queue_edit(index, capabilities);
        }
        let submission = self.allocate_submission_id()?;
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .queue_message(submission)
    }

    /// Adds a ready, non-deleted remote row to the quote sources of a later
    /// reply, or removes it when it is already selected. This changes only
    /// session-local composer state; a Graph command is still created only
    /// after an explicit send.
    pub(super) fn toggle_quote_source(
        &mut self,
        index: usize,
        message_index: usize,
    ) -> Result<(), RemoteSendError> {
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .toggle_quote_source(message_index)
    }

    pub(super) fn quote_reply_view(
        &self,
        index: usize,
        message_index: usize,
    ) -> Option<RemoteChatQuoteReplyView> {
        self.conversations
            .get(index)?
            .quote_reply_view(message_index)
    }

    /// Stages the one allowed remote chat-message mutation without changing
    /// the provider-projected row. The caller can only address a message that
    /// the selected history supplied with an opaque Microsoft 365 target.
    pub(super) fn queue_reaction(
        &mut self,
        index: usize,
        message_index: usize,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<RemoteChatReactionRequest> {
        self.conversations
            .get_mut(index)?
            .queue_reaction(message_index, reaction, selected)
    }

    pub(super) fn reaction_view(
        &self,
        index: usize,
        message_index: usize,
        reaction: ReactionKind,
    ) -> Option<RemoteChatReactionView> {
        self.conversations
            .get(index)?
            .reaction_view(message_index, reaction)
    }

    /// A process-local key identifying one presentation row by its opaque
    /// provider target, stable across history refreshes that reorder or grow
    /// the list. `None` when the row has no target.
    /// Whether one conversation's loaded history has a row with this opaque
    /// message target.
    pub(super) fn has_message_target(&self, index: usize, target: &ProviderResourceRef) -> bool {
        self.conversations.get(index).is_some_and(|conversation| {
            conversation
                .message_reaction_metadata
                .iter()
                .any(|metadata| metadata.target.as_ref() == Some(target))
        })
    }

    pub(super) fn message_focus_key(&self, index: usize, message_index: usize) -> Option<u64> {
        self.conversations
            .get(index)?
            .message_focus_key(message_index)
    }

    pub(super) fn reaction_views(
        &self,
        index: usize,
        message_index: usize,
    ) -> Option<Vec<RemoteChatReactionView>> {
        self.conversations.get(index)?.reaction_views(message_index)
    }

    /// Returns the normalized priority for one current presentation row. The
    /// list position is never a provider target; it only stays aligned with
    /// the safe `Message` row rendered by this feature.
    pub(super) fn message_importance(
        &self,
        index: usize,
        message_index: usize,
    ) -> Option<RemoteChatMessageImportance> {
        self.conversations
            .get(index)?
            .message_importance(message_index)
    }

    pub(super) fn bind_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.conversations
            .iter_mut()
            .any(|conversation| conversation.bind_submission(submission, intent_id))
    }

    /// Binds a controller intent to the exact chat/message route that emitted
    /// it. The conversation target is checked in addition to the message
    /// target so an identically shaped ID in another chat cannot receive a
    /// terminal outcome.
    pub(super) fn bind_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        chat: ProviderResourceRef,
        message: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| {
                conversation.target == chat
                    && conversation.bind_reaction(intent_id, &message, reaction)
            })
    }

    /// Whether any visible or hidden conversation owns this reaction intent.
    /// A confirmed delivery, a scheduled retry and an uncertain outcome all keep
    /// the exact gate until a matching selected history reconciles it.
    pub(super) fn owns_reaction_intent_anywhere(&self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter()
            .chain(&self.hidden_conversations)
            .any(|conversation| conversation.has_reaction_intent(intent_id))
    }

    /// A known rejection may unlock only its originating exact message gate.
    pub(super) fn resolve_reaction_terminal_failure(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_reaction_terminal_failure(intent_id))
    }

    /// Drops only clicks that have not reached the app-owned controller.
    /// A bounded directory refresh can temporarily omit a chat; an already
    /// bound non-idempotent mutation must remain gated in its hidden
    /// tombstone until Graph provides a terminal outcome or a later
    /// target-bound history can reconcile it.
    pub(super) fn clear_unbound_pending_reactions(&mut self) {
        for conversation in &mut self.conversations {
            conversation.clear_unbound_pending_reactions();
        }
        for conversation in &mut self.hidden_conversations {
            conversation.clear_unbound_pending_reactions();
        }
    }

    /// A navigation or projection-loss boundary may discard only a quote
    /// action that the app has not yet bound to its controller intent. A
    /// bound non-idempotent request stays available for exact outcome routing.
    pub(super) fn clear_unbound_quote_replies(&mut self) {
        for conversation in &mut self.conversations {
            conversation.clear_unbound_quote_reply();
        }
        for conversation in &mut self.hidden_conversations {
            conversation.clear_unbound_quote_reply();
        }
    }

    pub(super) fn resolve_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_delivered(intent_id))
    }

    pub(super) fn resolve_retry_scheduled(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter()
            .chain(&self.hidden_conversations)
            .any(|conversation| conversation.has_intent(intent_id))
    }

    pub(super) fn resolve_terminal_failure(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_terminal_failure(intent_id))
    }

    pub(super) fn resolve_delivery_uncertain(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_delivery_uncertain(intent_id))
    }

    pub(super) fn complete_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteReconciliationResolution> {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .find_map(|conversation| conversation.complete_reconciliation(intent_id))
    }

    pub(super) fn fail_reconciliation(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.fail_reconciliation(intent_id))
    }

    pub(super) fn has_submission(&self, submission: RemoteSubmissionId) -> bool {
        self.conversations
            .iter()
            .any(|conversation| conversation.has_submission(submission))
    }

    /// An unbound quote action remains valid only for the current selected,
    /// ready history that supplied its exact opaque source target. A stale
    /// timeline index, deleted row, directory preview, or changed selection
    /// cannot become a late Graph quote reply.
    pub(super) fn has_dispatchable_quote_submission(
        &self,
        selected_index: Option<usize>,
        submission: RemoteSubmissionId,
        chat: &ProviderResourceRef,
        messages: &[ProviderResourceRef],
    ) -> bool {
        selected_index
            .and_then(|index| self.conversations.get(index))
            .is_some_and(|conversation| {
                conversation.target == *chat
                    && conversation.has_dispatchable_quote_submission(submission, messages)
            })
    }

    /// Reports only a reaction action that can still cross from the feature
    /// into the app-owned controller. A bound tombstone, an unselected chat,
    /// a stale history row, or an already-matching provider state must never
    /// make an old feature action dispatchable after a projection replacement.
    pub(super) fn has_dispatchable_reaction(
        &self,
        selected_index: Option<usize>,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        selected_index
            .and_then(|index| self.conversations.get(index))
            .is_some_and(|conversation| {
                conversation.target == *chat
                    && conversation.has_dispatchable_reaction(message, reaction, selected)
            })
    }

    /// Removes the unbound gate belonging to a reaction click that was pruned
    /// before dispatch. It matches the exact chat, message, kind and requested
    /// state, and never removes a gate already bound to a controller intent.
    pub(super) fn discard_unbound_reaction(
        &mut self,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) {
        for conversation in self
            .conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .filter(|conversation| conversation.target == *chat)
        {
            conversation.pending_reactions.retain(|pending| {
                pending.intent_id.is_some()
                    || pending.message != *message
                    || pending.reaction != reaction
                    || pending.selected != selected
            });
        }
    }

    fn allocate_submission_id(&mut self) -> Result<RemoteSubmissionId, RemoteSendError> {
        let next = self
            .next_submission_id
            .checked_add(1)
            .ok_or(RemoteSendError::IdentifierExhausted)?;
        self.next_submission_id = next;
        Ok(RemoteSubmissionId(next))
    }
}

fn take_conversation_for_target(
    conversations: &mut Vec<RemoteConversation>,
    target: &ProviderResourceRef,
) -> Option<RemoteConversation> {
    conversations
        .iter()
        .position(|conversation| conversation.target == *target)
        .map(|index| conversations.swap_remove(index))
}

/// Feature-local identity that connects a pending remote composer to an
/// integration-owned intent. Its number has no provider meaning and cannot
/// be used to address a Graph resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteSubmissionId(u64);

/// A remote send accepted by the feature but not yet acknowledged by the
/// transport. Its targets remain opaque outside the integrations seam.
pub(super) enum RemoteSendRequest {
    Message {
        submission: RemoteSubmissionId,
        target: ProviderResourceRef,
        body: ComposedBody,
        importance: RemoteChatMessageImportance,
        attachment: Option<UploadedFileHandle>,
    },
    Edit {
        submission: RemoteSubmissionId,
        target: ProviderResourceRef,
        message: ProviderResourceRef,
        body: ComposedBody,
    },
    Delete {
        submission: RemoteSubmissionId,
        target: ProviderResourceRef,
        message: ProviderResourceRef,
    },
    QuoteReply {
        submission: RemoteSubmissionId,
        target: ProviderResourceRef,
        quoted_messages: Vec<ProviderResourceRef>,
        body: String,
    },
}

/// One exact, provider-owned chat-message reaction request. It remains
/// feature-local until the Chat facade turns it into a provider-neutral
/// action, so no local timeline or durable reaction state can be altered.
pub(super) struct RemoteChatReactionRequest {
    pub(super) chat: ProviderResourceRef,
    pub(super) message: ProviderResourceRef,
    pub(super) reaction: ReactionKind,
    pub(super) selected: bool,
}

/// A display-safe projection of one bounded remote reaction. The opaque target
/// remains inside `RemoteConversation`, never in the UI.
#[derive(Clone, Copy)]
pub(super) struct RemoteChatReactionView {
    pub(super) reaction: ReactionKind,
    pub(super) count: usize,
    pub(super) selected: bool,
    pub(super) pending: bool,
    pub(super) can_react: bool,
}

/// Display-safe availability for a per-row quote action. The opaque message
/// target stays in `RemoteConversation`; the UI can only ask to begin a quote
/// for the current row index and the feature rechecks the target-bound state.
#[derive(Clone, Copy)]
pub(super) struct RemoteChatQuoteReplyView {
    /// The row can be added to, or removed from, the quote set right now.
    pub(super) can_quote: bool,
    /// The row is currently one of the quote sources.
    pub(super) selected: bool,
    /// The quote set is full, so an unselected row cannot be added.
    pub(super) limit_reached: bool,
}

/// Provider-owned per-message metadata kept parallel to the safe rendering
/// model. Rows with no valid provider message ID remain visible but cannot
/// become a Graph reaction target. Current-user mentions and message priority
/// are already normalized by the integration layer, so this feature never
/// guesses at recipients from rendered HTML/display names or exposes raw
/// provider strings.
struct RemoteMessageReactionMetadata {
    target: Option<ProviderResourceRef>,
    /// The source targets of the strictly normalized Graph
    /// `messageReference` set, if any. They never cross to generic UI code;
    /// they only prevent an ambiguous quote reply from matching body text.
    quoted_messages: Vec<ProviderResourceRef>,
    deleted: bool,
    /// The row was sent by the verified signed-in account.
    own: bool,
    /// The adapter proved the whole body can be rewritten as plain text.
    plain_text_editable: bool,
    mentioned_current_user: bool,
    /// The members this message mentions, as opaque handles, for confirming
    /// an ambiguous mention send. Never shown.
    mentioned_handles: Vec<MentionHandle>,
    importance: RemoteChatMessageImportance,
    reactions: Vec<RemoteReactionSummary>,
    /// The attachment IDs of the file references the row carries. Never
    /// shown; used only to confirm an ambiguous file send.
    file_references: Vec<String>,
    /// The row has no text of its own (a file-only message).
    empty_text: bool,
}

/// Provider-normalized reaction data copied into the feature's opaque
/// presentation metadata. Keeping this small value type here avoids handing
/// integration-owned container types to UI state while preserving a typed
/// reaction-kind boundary.
#[derive(Clone, Copy)]
struct RemoteReactionSummary {
    reaction: ReactionKind,
    count: usize,
    selected: bool,
}

impl RemoteMessageReactionMetadata {
    /// An absent supported reaction means nobody in the bounded provider
    /// projection selected it. This lets an unset action reconcile when the
    /// corresponding entry disappears from the next selected-history read.
    fn reaction_state(&self, reaction: ReactionKind) -> (usize, bool) {
        self.reactions
            .iter()
            .find(|summary| summary.reaction == reaction)
            .map(|summary| (summary.count, summary.selected))
            .unwrap_or((0, false))
    }
}

/// An exact chat/message reaction awaiting provider reconciliation. Seeing the
/// same row again is insufficient: the projected selection must match this
/// request before it is released.
struct PendingRemoteChatReaction {
    message: ProviderResourceRef,
    reaction: ReactionKind,
    selected: bool,
    intent_id: Option<WorkspaceIntentId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteSendError {
    Missing,
    QuoteUnavailable,
    /// The quote set already holds the Graph maximum.
    QuoteLimit,
    /// Quote mode and edit mode refuse each other.
    QuoteBlockedByEdit,
    EditBlockedByQuote,
    Empty,
    Pending,
    TooLong,
    IdentifierExhausted,
    /// The row is no longer an own, editable message of the current history.
    EditUnavailable,
    /// The row's formatting cannot be written back losslessly here.
    EditFormatting,
    DeleteUnavailable,
    EditNeedsEmptyDraft,
    /// Saving an edit that changes nothing would only add a pointless write.
    Unchanged,
    /// The attachment is still uploading.
    AttachmentUploading,
    /// A file found after an unknown upload outcome must be accepted first.
    AttachmentNotAccepted,
    /// An attachment cannot go with a quote reply or an edit.
    AttachmentBlockedByMode,
}

impl RemoteSendError {
    pub(super) const fn user_message(self) -> &'static str {
        match self {
            Self::Missing => "That Microsoft 365 chat is no longer available.",
            Self::QuoteUnavailable => "That Microsoft 365 message is no longer available to quote.",
            Self::QuoteLimit => "You can quote up to 10 messages in one reply.",
            Self::QuoteBlockedByEdit => "Finish or cancel your edit before quoting a message.",
            Self::EditBlockedByQuote => "Cancel your quote before editing a message.",
            Self::Empty => "A message cannot be empty.",
            Self::Pending => "Wait for the previous Microsoft 365 message to finish.",
            Self::TooLong => "That message is too long to send to Microsoft 365.",
            Self::IdentifierExhausted => "This session cannot allocate another remote message.",
            Self::EditUnavailable => "That Microsoft 365 message can no longer be edited.",
            Self::EditFormatting => {
                "This message's formatting can't be edited here. You can still delete it."
            }
            Self::DeleteUnavailable => "That Microsoft 365 message can no longer be deleted.",
            Self::EditNeedsEmptyDraft => "Send or clear your draft before editing a message.",
            Self::Unchanged => "Change the message before saving the edit.",
            Self::AttachmentUploading => "Wait for the file to finish uploading.",
            Self::AttachmentNotAccepted => {
                "Choose Attach on the file Microsoft 365 may already have, or remove it, before sending."
            }
            Self::AttachmentBlockedByMode => {
                "Remove the attachment first: a file cannot go with a quote or an edit."
            }
        }
    }
}

/// A feature-owned view of a normalized remote conversation.
///
/// `target` is opaque to the feature. It can be preserved and passed into a
/// provider-neutral command, but no display label or fixture key can be
/// rebranded as a Microsoft 365 ID.
pub(super) struct RemoteConversation {
    target: ProviderResourceRef,
    kind: RemoteChatKind,
    title: String,
    subtitle: String,
    initials: String,
    color: Color32,
    unread: bool,
    /// The provider's unread indicator from the newest directory snapshot.
    provider_unread: bool,
    /// Opaque target of the directory preview message in the newest snapshot.
    preview_target: Option<ProviderResourceRef>,
    /// Local, session-only acknowledgement: the newest message the user has
    /// seen. The chat reads as unread again only when the provider reports
    /// unread AND the preview message differs from this one; an unknown
    /// target never proves it was read. It travels with the opaque target
    /// over a projection refresh, but is never persisted.
    read_through: Option<ProviderResourceRef>,
    /// Time of the directory preview message, when Graph gave a parsable one.
    preview_instant: Option<RemoteMessageInstant>,
    /// The preview is the user's own message or a deleted one, so it never
    /// announces itself as a new arrival.
    preview_suppressed: bool,
    /// The newest preview already seen; arrivals must be strictly later.
    notify_watermark: Option<(ProviderResourceRef, RemoteMessageInstant)>,
    /// Arrivals announced since the chat was last read, for the sidebar badge.
    new_count: usize,
    /// The last refresh of already-loaded history failed; the rows shown are
    /// stale. Cleared by the next successful read.
    refresh_failed: bool,
    messages: Vec<Message>,
    /// Directory-derived sidebar summary: the newest message text and time.
    /// It always follows the freshest projection, even while the selected
    /// chat keeps older loaded rows visible during a reload.
    preview_text: String,
    preview_time: String,
    /// Lowercased title, subtitle, preview and message text, built whenever
    /// any of them changes so per-frame sidebar/search filtering is a single
    /// `contains` instead of re-lowercasing every message each frame.
    search_key: String,
    /// Whether any non-deleted message of the last loaded selected history
    /// mentioned the current user. A directory preview carries no mentions,
    /// so this is kept across refreshes until a newer message than
    /// `history_newest_message` appears.
    history_mentions_current_user: bool,
    history_newest_message: Option<ProviderResourceRef>,
    /// Whether `messages` came from an exact selected-history read, so a
    /// directory refresh may keep them (stale) while a reload runs.
    rows_from_selected_history: bool,
    /// A selected-history lifecycle is deliberately separate from the bounded
    /// chat-directory snapshot. It is tied to this opaque target, survives no
    /// profile boundary, and never creates a local record or read receipt.
    history_state: RemoteChatHistoryState,
    /// Parallel to `messages`; this preserves only the opaque metadata needed
    /// for bounded, direct typed reaction actions while keeping presentation
    /// content in the existing `Message` model.
    message_reaction_metadata: Vec<RemoteMessageReactionMetadata>,
    pending_reactions: Vec<PendingRemoteChatReaction>,
    /// At most one edit or deletion of an own message in flight. Session
    /// only; it never changes the provider-owned rows.
    mutation: Option<PendingRemoteMessageMutation>,
    /// An explicit, read-only panel for Graph-pinned messages. It contains
    /// presentation-safe copies only and is discarded at every selection,
    /// projection-loss, disconnect, or account boundary.
    pinned_messages: RemotePinnedMessagesState,
    /// An explicit, read-only People panel for the selected Microsoft 365
    /// chat. It retains only presentation-safe name/role copies and is
    /// discarded at every selection, projection-loss, disconnect, or account
    /// boundary.
    members: RemoteChatMembersState,
    /// The members the `@` picker may offer: the last People roster read for
    /// this chat that said who can be mentioned. It outlives the People
    /// panel (the adapter keeps the matching directory), and is session-only.
    mention_roster: Vec<DraftMention>,
    /// When the next presence-only refresh may be requested. Session-only,
    /// set only while People is open and cleared with it.
    presence_due: Option<std::time::Instant>,
    composer: RemoteComposerState,
}

/// Presentation state for a selected chat's explicit bounded history read.
/// `Ready` intentionally carries no provider data itself: normalized messages
/// remain on the owning conversation, preserving the feature's existing
/// opaque-target and remote-composer lifecycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RemoteChatHistoryState {
    #[default]
    NotRequested,
    Loading,
    AccessDenied,
    Ready,
    /// Rows from an earlier exact read stay visible, but newer activity or a
    /// failed refresh means they are not current proof. Not `Ready`: every
    /// reaction, quote and edit gate stays closed.
    Stale,
    /// A refresh of the stale rows is in flight.
    Refreshing,
}

fn rows_have_matching_own_message(
    messages: &[Message],
    metadata: &[RemoteMessageReactionMetadata],
    body: &ComposedBody,
    importance: RemoteChatMessageImportance,
    quoted_messages: &[ProviderResourceRef],
    attachment: Option<&str>,
) -> bool {
    let expected_mentions = body.mentions();
    messages.iter().zip(metadata).any(|(message, metadata)| {
        message.own
            && same_handles(&metadata.mentioned_handles, &expected_mentions)
            && body_matches_text(
                body,
                &message.text,
                metadata.empty_text,
                attachment.is_some(),
            )
            && metadata.importance == importance
            && same_reference_set(&metadata.quoted_messages, quoted_messages)
            && same_file_references(&metadata.file_references, attachment)
    })
}

/// A file send is confirmed only by a row that carries exactly that file's
/// attachment ID, and a send without a file only by a row with no file, so a
/// plain message and a file message can never confirm each other.
fn same_file_references(found: &[String], expected: Option<&str>) -> bool {
    match expected {
        Some(id) => found.len() == 1 && found[0] == id,
        None => found.is_empty(),
    }
}

/// Multiset equality of mention handles, order ignored but repeats counted:
/// mentioning someone twice is not the same message as mentioning them once,
/// and a plain body never matches a row that mentions anyone.
fn same_handles(left: &[MentionHandle], right: &[MentionHandle]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut remaining = right.to_vec();
    left.iter().all(|handle| {
        remaining
            .iter()
            .position(|other| other == handle)
            .map(|index| remaining.swap_remove(index))
            .is_some()
    })
}

/// A plain body is confirmed only by exactly its text. A formatted body comes
/// back from Graph as HTML, whose plain projection collapses layout, so it is
/// compared on its visible words alone.
///
/// A file message always comes back as HTML, so its text is compared on its
/// visible words; a file with no text matches only a row with no text.
fn body_matches_text(
    body: &ComposedBody,
    text: &str,
    row_text_is_empty: bool,
    with_attachment: bool,
) -> bool {
    let projection = body.plain_projection();
    if projection.trim().is_empty() {
        return with_attachment && row_text_is_empty;
    }
    if body.is_plain() && !with_attachment {
        projection == text
    } else {
        same_visible_text(&projection, text)
    }
}

/// Set equality for opaque quote sources: same size and members, order
/// ignored. Normalization rejects duplicates, so size plus containment is
/// exact. An empty set equals only an empty set, so a plain message and a
/// quote reply can never confirm each other.
fn same_reference_set(left: &[ProviderResourceRef], right: &[ProviderResourceRef]) -> bool {
    left.len() == right.len() && left.iter().all(|target| right.contains(target))
}

fn presentation_messages(
    messages: &[RemoteChatMessage],
    conversation_color: Color32,
) -> (Vec<Message>, Vec<RemoteMessageReactionMetadata>) {
    let clock = local_time::clock();
    messages
        .iter()
        .map(|message| {
            let reactions = message
                .reactions()
                .iter()
                .map(|reaction| RemoteReactionSummary {
                    reaction: reaction.kind(),
                    count: reaction.count(),
                    selected: reaction.reacted_by_current_user(),
                })
                .collect();
            (
                Message {
                    name: nonempty_or(message.sender_name(), "Microsoft 365 user"),
                    time: match message.sent_instant() {
                        Some(instant) => local_time::label(instant, &clock)
                            .unwrap_or_else(|| nonempty_or(message.sent_at(), "Unknown time")),
                        None => nonempty_or(message.sent_at(), "Unknown time"),
                    },
                    text: if message.is_deleted() {
                        "This message was deleted.".into()
                    } else {
                        nonempty_or(message.plain_text(), "No message content")
                    },
                    color: if message.sent_by_current_user() {
                        theme::brand()
                    } else {
                        conversation_color
                    },
                    own: message.sent_by_current_user(),
                    attachment: None,
                    day_divider_before: false,
                    reply_context: message.quotes().first().map(|quote| MessageReply {
                        name: nonempty_or(quote.sender_name(), "Microsoft 365 user"),
                        // Graph's messageReference payload does not carry a
                        // trustworthy sent time. Keep it empty so the shared
                        // renderer shows no invented timestamp.
                        time: String::new(),
                        text: nonempty_or(quote.preview(), "Quoted message"),
                    }),
                    reply_context_extra: message.quotes().len().saturating_sub(1),
                    file_chips: message
                        .attachments()
                        .iter()
                        .map(|attachment| attachment.name().to_owned())
                        .collect(),
                    // Remote reaction state is rendered by the specialized
                    // control below, not the generic local reaction pills.
                    // That keeps a pending provider mutation visibly gated.
                    reactions: Vec::new(),
                    rich: if message.is_deleted() {
                        None
                    } else {
                        message.rich().map(<[_]>::to_vec)
                    },
                },
                RemoteMessageReactionMetadata {
                    target: message.target(),
                    quoted_messages: message
                        .quotes()
                        .iter()
                        .map(|quote| quote.target())
                        .collect(),
                    deleted: message.is_deleted(),
                    own: message.sent_by_current_user(),
                    plain_text_editable: message.is_plain_text_editable(),
                    mentioned_current_user: message.mentions_current_user(),
                    mentioned_handles: message.mentioned_handles().to_vec(),
                    importance: message.importance(),
                    reactions,
                    file_references: message
                        .attachments()
                        .iter()
                        .map(|attachment| attachment.id().to_owned())
                        .collect(),
                    empty_text: message.plain_text().trim().is_empty() && !message.is_deleted(),
                },
            )
        })
        .unzip()
}

const MAX_REMOTE_MESSAGE_TEXT_BYTES: usize = 28 * 1024;

/// The most distinct members one draft can hold (Graph allows 20 mentions).
const MAX_DRAFT_MENTIONS: usize = 20;

/// A compact presentation-only model for a Graph-pinned message. The
/// integration layer has already bounded and converted all provider content
/// to inert text; the feature deliberately retains no opaque message target,
/// attachment, reaction, or mutation affordance for this read-only panel.
#[derive(Clone)]
pub(super) struct RemotePinnedMessage {
    pub(super) sender_name: String,
    pub(super) sent_at: String,
    pub(super) plain_text: String,
    pub(super) deleted: bool,
}

/// A cloneable view of the selected chat's contextual pinned-message panel.
/// `Closed` deliberately has no public variant because callers must not draw
/// an overlay after the user has dismissed it or changed chat selection.
pub(super) enum RemotePinnedMessagesPanel<'a> {
    Loading,
    AccessDenied,
    /// The read failed transiently. Closing and reopening the panel retries.
    Unavailable,
    Ready(&'a [RemotePinnedMessage]),
}

#[derive(Default)]
enum RemotePinnedMessagesState {
    #[default]
    Closed,
    Loading,
    AccessDenied,
    Unavailable,
    Ready(Vec<RemotePinnedMessage>),
}

/// A compact presentation-only model for a Graph chat member. The integration
/// layer has already discarded member IDs, email, tenant data, unknown roles,
/// and raw DTO fields; the feature preserves no action capability.
#[derive(Clone)]
pub(super) struct RemoteChatMemberView {
    pub(super) display_name: String,
    pub(super) role_label: Option<String>,
    pub(super) presence: Option<RemoteMemberPresence>,
}

/// The minimum gap between presence refreshes of an open People panel.
pub(super) const PRESENCE_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// What the shell should do about the presence refresh right now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PresenceSchedule {
    Idle,
    /// Check again after this long.
    Wait(std::time::Duration),
    Request(ProviderResourceRef),
}

/// A cloneable view of the selected chat's contextual People panel. `Closed`
/// deliberately has no public variant because callers must not draw an
/// overlay after dismissal or a chat-selection transition.
pub(super) enum RemoteChatMembersPanel<'a> {
    Loading,
    AccessDenied,
    /// The read failed transiently. Closing and reopening the panel retries.
    Unavailable,
    Ready(&'a [RemoteChatMemberView]),
}

#[derive(Default)]
enum RemoteChatMembersState {
    #[default]
    Closed,
    Loading,
    AccessDenied,
    Unavailable,
    Ready(Vec<RemoteChatMemberView>),
}

#[derive(Default)]
struct RemoteComposerState {
    draft: String,
    /// The selected priority is session-only and resets to Normal after a
    /// confirmed delivery. Pending/reconciliation records retain their own
    /// value so a rejected, cancelled, or uncertain send can restore it.
    importance: RemoteChatMessageImportance,
    /// The selected source rows (at most ten) for the next remote quote
    /// reply. This is session-only and contains only opaque targets plus
    /// already-safe display context; it is never persisted or sent as
    /// generic metadata.
    quote: Option<RemoteQuoteSelection>,
    /// A one-shot explanation shown after sources were dropped because they
    /// vanished from history. The UI drains it into a toast.
    quote_notice: Option<&'static str>,
    /// Set while the composer holds the edit text of one own message.
    editing: Option<RemoteEditSelection>,
    /// Members picked with the `@` picker whose `@Name` may still be in the
    /// draft. At send, only the intact occurrences become mentions.
    draft_mentions: Vec<DraftMention>,
    pending: Option<PendingRemoteSend>,
    awaiting_reconciliation: Option<AwaitingRemoteReconciliation>,
    /// The file for the next send; see `attachment.rs`.
    attachment: Option<ComposerAttachment>,
}

struct RemoteQuoteSource {
    target: ProviderResourceRef,
    context: MessageReply,
}

const QUOTE_SOURCES_GONE_NOTICE: &str =
    "A quoted Microsoft 365 message is no longer available, so it was removed from your quote.";

impl RemoteQuoteSelection {
    fn targets(&self) -> Vec<ProviderResourceRef> {
        self.sources
            .iter()
            .map(|source| source.target.clone())
            .collect()
    }
}

struct RemoteQuoteSelection {
    /// Distinct sources in the order the user picked them.
    sources: Vec<RemoteQuoteSource>,
    /// The ordinary composer priority is retained so cancelling an unsent
    /// quote restores exactly the user's pre-quote choice. Graph quote
    /// payloads themselves intentionally stay Normal.
    priority_before_quote: RemoteChatMessageImportance,
}

struct PendingRemoteSend {
    submission: RemoteSubmissionId,
    body: ComposedBody,
    /// The exact composer text the body was built from, so a rejected or
    /// uncertain send restores the user's own markup, not a re-rendering.
    draft_source: String,
    draft_mentions: Vec<DraftMention>,
    importance: RemoteChatMessageImportance,
    quote: Option<RemoteQuoteSelection>,
    /// The uploaded file this send carries. A rejected, cancelled or
    /// uncertain send restores it with the draft.
    attachment: Option<UploadedFileHandle>,
    intent_id: Option<WorkspaceIntentId>,
    cancellation_requested: bool,
}

/// The POST may have reached Microsoft Graph even though the client did not
/// receive a receipt. The composer remains locked until the controller's
/// exact-chat `ChatMessages` read resolves this state.
struct AwaitingRemoteReconciliation {
    intent_id: WorkspaceIntentId,
    body: ComposedBody,
    draft_source: String,
    draft_mentions: Vec<DraftMention>,
    importance: RemoteChatMessageImportance,
    quote: Option<RemoteQuoteSelection>,
    attachment: Option<UploadedFileHandle>,
    /// A matching own message already visible before the POST cannot prove
    /// that the ambiguous request succeeded. Treat such a snapshot as
    /// unconfirmed rather than silently discarding the user's draft.
    baseline_had_matching_own_message: bool,
    confirmed_in_projection: bool,
}

/// What an ambiguous send is looked for by in a fresh history read.
struct ReconciliationProbe {
    baseline_had_matching_own_message: bool,
    body: ComposedBody,
    importance: RemoteChatMessageImportance,
    quoted_messages: Vec<ProviderResourceRef>,
    /// The attachment ID of the file the send carried, if any.
    attachment: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteReconciliationResolution {
    Confirmed,
    DraftRestored,
}

impl RemoteConversation {
    fn from_projection(conversation: &RemoteChatConversation) -> Self {
        let title = nonempty_or(conversation.title(), "Microsoft 365 chat");
        let kind = conversation.kind();
        let subtitle = remote_subtitle(kind, conversation.subtitle());
        let initials = remote_initials(conversation.initials(), &title);
        let color = remote_color(&title);
        let (messages, message_reaction_metadata) =
            presentation_messages(conversation.messages(), color);
        let mut remote = Self {
            target: conversation.target(),
            kind,
            title,
            subtitle,
            initials,
            color,
            unread: conversation.is_unread(),
            provider_unread: conversation.is_unread(),
            preview_target: None,
            preview_instant: None,
            preview_suppressed: false,
            notify_watermark: None,
            new_count: 0,
            read_through: None,
            refresh_failed: false,
            messages,
            preview_text: String::new(),
            preview_time: String::new(),
            search_key: String::new(),
            history_mentions_current_user: false,
            history_newest_message: None,
            rows_from_selected_history: false,
            history_state: RemoteChatHistoryState::NotRequested,
            message_reaction_metadata,
            pending_reactions: Vec::new(),
            mutation: None,
            pinned_messages: RemotePinnedMessagesState::default(),
            members: RemoteChatMembersState::default(),
            mention_roster: Vec::new(),
            presence_due: None,
            composer: RemoteComposerState::default(),
        };
        remote.preview_target = remote.newest_message_target().cloned();
        if let Some(newest) = conversation.messages().last() {
            remote.preview_instant = newest.sent_instant();
            remote.preview_suppressed = newest.sent_by_current_user() || newest.is_deleted();
        }
        remote.refresh_preview();
        remote.rebuild_search_key();
        remote
    }

    /// Recomputes the sidebar summary from the rows currently held.
    fn refresh_preview(&mut self) {
        match self.messages.last() {
            Some(message) => {
                self.preview_text.clone_from(&message.text);
                self.preview_time.clone_from(&message.time);
            }
            None => {
                self.preview_text.clone_from(&self.subtitle);
                self.preview_time.clear();
            }
        }
    }

    fn rebuild_search_key(&mut self) {
        let mut key = String::new();
        for part in [&self.title, &self.subtitle, &self.preview_text] {
            key.push_str(&part.to_lowercase());
            key.push('\n');
        }
        for message in &self.messages {
            key.push_str(&message.text.to_lowercase());
            key.push('\n');
        }
        self.search_key = key;
    }

    /// The opaque target of the newest row that has one.
    fn newest_message_target(&self) -> Option<&ProviderResourceRef> {
        self.message_reaction_metadata
            .last()
            .and_then(|metadata| metadata.target.as_ref())
    }

    fn history_has_current_user_mention(&self) -> bool {
        self.message_reaction_metadata
            .iter()
            .any(|metadata| metadata.mentioned_current_user && !metadata.deleted)
    }

    fn inherit_session_state(
        &mut self,
        previous: RemoteConversation,
        keep_loaded_history: bool,
        viewing: bool,
    ) {
        self.composer = previous.composer;
        self.pending_reactions = previous.pending_reactions;
        self.mutation = previous.mutation;
        // The panel belongs to this exact opaque chat target. A directory
        // refresh can reorder the same chat, so preserve it here; a selection
        // change, omitted target, disconnect, or worker loss clears it at the
        // owning feature lifecycle instead.
        self.pinned_messages = previous.pinned_messages;
        self.members = previous.members;
        self.mention_roster = previous.mention_roster;
        self.presence_due = previous.presence_due;
        // The new directory snapshot owns `messages`, so a completed selected
        // history must not keep advertising itself as ready after those rows
        // were replaced. An in-flight selected read remains meaningful and is
        // retained until its target-bound result either arrives or is
        // cancelled by a selection/account transition.
        // Rows of a completed selected history stay (stale) across a
        // directory replace. They remain Ready only while the fresh preview
        // proves the newest message is unchanged; otherwise they are Stale,
        // which keeps every reaction, quote and edit control disabled and
        // asks the owning feature for a fresh exact read.
        let rows_kept = keep_loaded_history && previous.rows_from_selected_history;
        let preview_unchanged = self.preview_target.is_some()
            && self.preview_target.as_ref() == previous.history_newest_message.as_ref();
        self.history_state = match previous.history_state {
            RemoteChatHistoryState::Loading => RemoteChatHistoryState::Loading,
            RemoteChatHistoryState::Refreshing if rows_kept => RemoteChatHistoryState::Refreshing,
            RemoteChatHistoryState::Ready if rows_kept && preview_unchanged => {
                RemoteChatHistoryState::Ready
            }
            RemoteChatHistoryState::Ready
            | RemoteChatHistoryState::Stale
            | RemoteChatHistoryState::Refreshing
                if rows_kept =>
            {
                RemoteChatHistoryState::Stale
            }
            _ => RemoteChatHistoryState::NotRequested,
        };
        self.refresh_failed = rows_kept && previous.refresh_failed;
        // A sidebar click is intentionally only a local visual
        // acknowledgement. Keep it with the opaque target when Graph
        // replaces or reorders a snapshot, rather than making an already-read
        // dot reappear on every refresh.
        self.notify_watermark = previous.notify_watermark;
        self.new_count = previous.new_count;
        self.read_through = previous.read_through;
        if viewing && self.read_through.is_some() {
            // The selected chat is being viewed (window focused), so a newer
            // arrival is read. While unfocused it stays unread.
            self.read_through = self.preview_target.clone();
        }
        self.recompute_unread();
        // The fresh preview carries no mentions, so keep the history-derived
        // bit only while no newer message than the one it was computed from
        // has appeared (an unknown id cannot prove that, so it is dropped).
        if previous.history_mentions_current_user
            && self.newest_message_target().is_some()
            && self.newest_message_target() == previous.history_newest_message.as_ref()
        {
            self.history_mentions_current_user = true;
            self.history_newest_message = previous.history_newest_message.clone();
        }
        // Keep the selected chat's loaded rows visible while its fresh
        // history read runs, rather than collapsing a long conversation to
        // the single directory preview row on every refresh. They are stale,
        // not proof: `history_state` is no longer Ready, which keeps every
        // reaction and quote control disabled until the exact history read
        // replaces them. Only the selected chat reloads, so any other chat
        // takes the fresh directory preview.
        if keep_loaded_history && previous.rows_from_selected_history {
            self.messages = previous.messages;
            self.message_reaction_metadata = previous.message_reaction_metadata;
            self.rows_from_selected_history = true;
        }
        // Only the rows may be stale: the sidebar preview/time stay those of
        // the fresh directory projection set by `from_projection`.
        self.rebuild_search_key();
        // A directory refresh may include a recent message, but it is not the
        // selected chat-history read that the action scheduled. Preserve the
        // gate until `apply_history` receives the exact `ChatMessages`
        // projection; otherwise an unrelated directory poll could enable a
        // duplicate click before the reaction refresh arrives.
    }

    pub(super) fn title(&self) -> &str {
        &self.title
    }

    pub(super) fn subtitle(&self) -> &str {
        &self.subtitle
    }

    pub(super) fn initials(&self) -> &str {
        &self.initials
    }

    pub(super) fn color(&self) -> Color32 {
        self.color
    }

    pub(super) const fn is_unread(&self) -> bool {
        self.unread
    }

    /// Arrivals announced since this chat was last read (0 when unknown).
    pub(super) const fn new_count(&self) -> usize {
        self.new_count
    }

    /// Reading a projected chat is only a local visual acknowledgement for
    /// this session. The Graph read-receipt mutation is intentionally not
    /// inferred from a sidebar click.
    pub(super) fn mark_read(&mut self) {
        self.unread = false;
        self.new_count = 0;
        self.read_through = self.newest_message_target().cloned();
    }

    fn recompute_unread(&mut self) {
        let read = self.preview_target.is_some() && self.preview_target == self.read_through;
        self.unread = self.provider_unread && !read;
        if !self.unread {
            self.new_count = 0;
        }
    }

    /// Starts a refresh that keeps the loaded rows visible. Only a completed
    /// or stale selected history can refresh; the rows stay non-actionable
    /// until the exact read replaces them.
    fn begin_history_refresh(&mut self) -> Option<ProviderResourceRef> {
        if !matches!(
            self.history_state,
            RemoteChatHistoryState::Ready | RemoteChatHistoryState::Stale
        ) {
            return None;
        }
        self.history_state = RemoteChatHistoryState::Refreshing;
        self.refresh_failed = false;
        Some(self.target.clone())
    }

    fn mark_read_if_unread(&mut self) -> bool {
        if !self.unread {
            return false;
        }
        self.mark_read();
        true
    }

    /// Opens a fresh, explicit pinned-message read for this exact chat. It is
    /// intentionally independent of selected history: merely opening the
    /// panel cannot mutate read state or reuse a directory preview.
    fn open_pinned_messages(&mut self) -> Option<ProviderResourceRef> {
        if !matches!(self.pinned_messages, RemotePinnedMessagesState::Closed) {
            return None;
        }
        self.pinned_messages = RemotePinnedMessagesState::Loading;
        Some(self.target.clone())
    }

    /// Closes the panel and removes its rows before the controller cancels a
    /// queued or in-flight read. Returning `false` makes repeated close paths
    /// harmless during selection and account transitions.
    fn close_pinned_messages(&mut self) -> bool {
        if matches!(self.pinned_messages, RemotePinnedMessagesState::Closed) {
            return false;
        }
        self.pinned_messages = RemotePinnedMessagesState::Closed;
        true
    }

    fn has_open_pinned_messages(&self) -> bool {
        !matches!(self.pinned_messages, RemotePinnedMessagesState::Closed)
    }

    fn pinned_messages_panel(&self) -> Option<RemotePinnedMessagesPanel<'_>> {
        match &self.pinned_messages {
            RemotePinnedMessagesState::Closed => None,
            RemotePinnedMessagesState::Loading => Some(RemotePinnedMessagesPanel::Loading),
            RemotePinnedMessagesState::AccessDenied => {
                Some(RemotePinnedMessagesPanel::AccessDenied)
            }
            RemotePinnedMessagesState::Unavailable => Some(RemotePinnedMessagesPanel::Unavailable),
            RemotePinnedMessagesState::Ready(messages) => {
                Some(RemotePinnedMessagesPanel::Ready(messages))
            }
        }
    }

    fn apply_pinned_messages(&mut self, projection: RemotePinnedChatMessagesProjection) {
        debug_assert_eq!(self.target, projection.chat());
        if projection.is_unavailable() {
            if matches!(self.pinned_messages, RemotePinnedMessagesState::Loading) {
                self.pinned_messages = RemotePinnedMessagesState::Unavailable;
            }
            return;
        }
        if projection.is_access_denied() {
            self.pinned_messages = RemotePinnedMessagesState::AccessDenied;
            return;
        }
        let clock = local_time::clock();
        self.pinned_messages = RemotePinnedMessagesState::Ready(
            projection
                .messages()
                .iter()
                .map(|message| RemotePinnedMessage {
                    sender_name: nonempty_or(message.sender_name(), "Microsoft 365 user"),
                    sent_at: local_time::label_or(
                        message.sent_instant(),
                        message.sent_at().trim(),
                        &clock,
                    ),
                    plain_text: if message.is_deleted() {
                        "This message was deleted.".into()
                    } else {
                        nonempty_or(message.plain_text(), "No message content")
                    },
                    deleted: message.is_deleted(),
                })
                .collect(),
        );
    }

    /// Opens a fresh, explicit member-roster read for this exact chat. It is
    /// independent of selected history and pinned messages: opening the panel
    /// cannot mutate chat state or reuse a directory preview.
    fn open_members(&mut self) -> Option<ProviderResourceRef> {
        if !matches!(self.members, RemoteChatMembersState::Closed) {
            return None;
        }
        self.members = RemoteChatMembersState::Loading;
        Some(self.target.clone())
    }

    /// Closes the panel and removes its rows before the controller cancels a
    /// queued or in-flight read. Repeated close paths are harmless.
    fn close_members(&mut self) -> bool {
        if matches!(self.members, RemoteChatMembersState::Closed) {
            return false;
        }
        self.members = RemoteChatMembersState::Closed;
        self.presence_due = None;
        true
    }

    fn has_open_members(&self) -> bool {
        !matches!(self.members, RemoteChatMembersState::Closed)
    }

    fn members_panel(&self) -> Option<RemoteChatMembersPanel<'_>> {
        match &self.members {
            RemoteChatMembersState::Closed => None,
            RemoteChatMembersState::Loading => Some(RemoteChatMembersPanel::Loading),
            RemoteChatMembersState::AccessDenied => Some(RemoteChatMembersPanel::AccessDenied),
            RemoteChatMembersState::Unavailable => Some(RemoteChatMembersPanel::Unavailable),
            RemoteChatMembersState::Ready(members) => Some(RemoteChatMembersPanel::Ready(members)),
        }
    }

    /// Applies a presence-only refresh to the rows already shown. Labels are
    /// kept on any failure, and the schedule continues only while it can work.
    fn apply_presence_update(&mut self, projection: &RemoteChatMembersProjection) {
        if let RemoteChatMembersState::Ready(rows) = &mut self.members {
            let updates = projection.presence_updates();
            if updates.len() == rows.len() {
                for (row, presence) in rows.iter_mut().zip(updates) {
                    row.presence = *presence;
                }
            } else if !projection.presence_status().keeps_refreshing() {
                for row in rows.iter_mut() {
                    row.presence = None;
                }
            }
        }
        self.presence_due = projection
            .presence_status()
            .keeps_refreshing()
            .then(|| std::time::Instant::now() + PRESENCE_REFRESH_INTERVAL);
    }

    /// Decides whether a refresh is due. It is held back while the window is
    /// unfocused, and a request consumes the deadline: the next one is armed
    /// only by the answer, so a lost or empty answer ends the refresh.
    fn presence_schedule(&mut self, now: std::time::Instant, focused: bool) -> PresenceSchedule {
        let Some(due) = self.presence_due else {
            return PresenceSchedule::Idle;
        };
        if !self.has_open_members() {
            self.presence_due = None;
            return PresenceSchedule::Idle;
        }
        if now < due {
            return PresenceSchedule::Wait(due - now);
        }
        if !focused {
            return PresenceSchedule::Idle;
        }
        self.presence_due = None;
        PresenceSchedule::Request(self.target.clone())
    }

    fn apply_members(&mut self, projection: RemoteChatMembersProjection) {
        debug_assert_eq!(self.target, projection.chat());
        if projection.is_presence_update() {
            self.apply_presence_update(&projection);
            return;
        }
        if projection.is_unavailable() {
            if matches!(self.members, RemoteChatMembersState::Loading) {
                self.members = RemoteChatMembersState::Unavailable;
            }
            return;
        }
        if projection.is_access_denied() {
            self.mention_roster.clear();
            self.members = RemoteChatMembersState::AccessDenied;
            self.presence_due = None;
            return;
        }
        self.mention_roster = projection
            .members()
            .iter()
            .filter_map(|member| {
                Some(DraftMention {
                    handle: member.mention()?,
                    display: member.display_name().to_owned(),
                })
            })
            .collect();
        self.members = RemoteChatMembersState::Ready(
            projection
                .members()
                .iter()
                .map(|member| RemoteChatMemberView {
                    display_name: nonempty_or(member.display_name(), "Microsoft 365 member"),
                    role_label: member
                        .role_label()
                        .map(str::trim)
                        .filter(|role| !role.is_empty())
                        .map(str::to_owned),
                    presence: member.presence(),
                })
                .collect(),
        );
        self.presence_due = projection
            .presence_status()
            .keeps_refreshing()
            .then(|| std::time::Instant::now() + PRESENCE_REFRESH_INTERVAL);
    }

    pub(super) fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns whether a non-deleted row in the current provider projection
    /// explicitly mentions the verified current account. This stays a
    /// presentation predicate only: it neither reads new data nor marks the
    /// chat read.
    pub(super) fn has_current_user_mention(&self) -> bool {
        self.history_mentions_current_user || self.history_has_current_user_mention()
    }

    fn message_focus_key(&self, message_index: usize) -> Option<u64> {
        self.message_reaction_metadata
            .get(message_index)?
            .target
            .as_ref()
            .map(ProviderResourceRef::stable_key)
    }

    fn message_importance(&self, message_index: usize) -> Option<RemoteChatMessageImportance> {
        let metadata = self.message_reaction_metadata.get(message_index)?;
        (!metadata.deleted).then_some(metadata.importance)
    }

    fn quote_reply_view(&self, message_index: usize) -> Option<RemoteChatQuoteReplyView> {
        let metadata = self.message_reaction_metadata.get(message_index)?;
        let selected = metadata.target.as_ref().is_some_and(|target| {
            self.composer.quote.as_ref().is_some_and(|selection| {
                selection
                    .sources
                    .iter()
                    .any(|source| source.target == *target)
            })
        });
        let limit_reached = self.composer.quote.as_ref().is_some_and(|selection| {
            selection.sources.len() >= crate::integrations::MAX_REMOTE_QUOTE_SOURCES
        });
        Some(RemoteChatQuoteReplyView {
            can_quote: matches!(self.history_state, RemoteChatHistoryState::Ready)
                && !self.has_pending_send()
                && !self.is_editing()
                && !metadata.deleted
                && metadata.target.is_some()
                && (selected || !limit_reached),
            selected,
            limit_reached,
        })
    }

    /// Adds the row to the quote sources, or removes it if already selected.
    /// The source target and its presentation context come from the same
    /// current history row; row text or an index can never be promoted into
    /// an opaque Graph target. Quote and edit mode refuse each other.
    fn toggle_quote_source(&mut self, message_index: usize) -> Result<(), RemoteSendError> {
        if self.is_editing() {
            return Err(RemoteSendError::QuoteBlockedByEdit);
        }
        if self.has_attachment() {
            return Err(RemoteSendError::AttachmentBlockedByMode);
        }
        if self.has_pending_send() {
            return Err(RemoteSendError::Pending);
        }
        if !matches!(self.history_state, RemoteChatHistoryState::Ready) {
            return Err(RemoteSendError::QuoteUnavailable);
        }
        let metadata = self
            .message_reaction_metadata
            .get(message_index)
            .ok_or(RemoteSendError::QuoteUnavailable)?;
        if metadata.deleted {
            return Err(RemoteSendError::QuoteUnavailable);
        }
        let target = metadata
            .target
            .clone()
            .ok_or(RemoteSendError::QuoteUnavailable)?;
        // Toggling off an already selected source.
        if let Some(position) = self.composer.quote.as_ref().and_then(|selection| {
            selection
                .sources
                .iter()
                .position(|source| source.target == target)
        }) {
            self.composer.quote_notice = None;
            self.remove_quote_source(position);
            return Ok(());
        }
        if self.composer.quote.as_ref().is_some_and(|selection| {
            selection.sources.len() >= crate::integrations::MAX_REMOTE_QUOTE_SOURCES
        }) {
            return Err(RemoteSendError::QuoteLimit);
        }
        let source = self
            .messages
            .get(message_index)
            .ok_or(RemoteSendError::QuoteUnavailable)?;
        let new_source = RemoteQuoteSource {
            target,
            context: MessageReply {
                name: source.name.clone(),
                time: source.time.clone(),
                text: source.text.clone(),
            },
        };
        self.composer.quote_notice = None;
        if let Some(selection) = self.composer.quote.as_mut() {
            selection.sources.push(new_source);
        } else {
            // Quote mode forces Normal priority; remember the user's choice
            // so cancelling restores it exactly.
            let priority_before_quote = self.composer.importance;
            self.composer.importance = RemoteChatMessageImportance::Normal;
            self.composer.quote = Some(RemoteQuoteSelection {
                sources: vec![new_source],
                priority_before_quote,
            });
        }
        Ok(())
    }

    /// Removes one unsent quote source by its position; removing the last
    /// one leaves quote mode and restores the pre-quote priority.
    pub(super) fn remove_quote_source(&mut self, position: usize) -> bool {
        if self.has_pending_send() {
            return false;
        }
        let Some(selection) = self.composer.quote.as_mut() else {
            return false;
        };
        if position >= selection.sources.len() {
            return false;
        }
        selection.sources.remove(position);
        if selection.sources.is_empty() {
            return self.cancel_quote_reply();
        }
        true
    }

    /// Cancels an unsubmitted quote selection and returns the normal composer
    /// to exactly the priority chosen before quote mode. Draft text remains in
    /// place for an ordinary remote message.
    pub(super) fn cancel_quote_reply(&mut self) -> bool {
        if self.has_pending_send() {
            return false;
        }
        let Some(selection) = self.composer.quote.take() else {
            return false;
        };
        self.composer.importance = selection.priority_before_quote;
        true
    }

    /// The display context of every quoted source, in selection order.
    pub(super) fn quote_reply_contexts(&self) -> Vec<&MessageReply> {
        self.composer
            .pending
            .as_ref()
            .and_then(|pending| pending.quote.as_ref())
            .or_else(|| {
                self.composer
                    .awaiting_reconciliation
                    .as_ref()
                    .and_then(|reconciliation| reconciliation.quote.as_ref())
            })
            .or(self.composer.quote.as_ref())
            .map(|selection| selection.sources.iter().map(|s| &s.context).collect())
            .unwrap_or_default()
    }

    /// Drains the one-shot notice explaining dropped quote sources.
    pub(super) fn take_quote_notice(&mut self) -> Option<&'static str> {
        self.composer.quote_notice.take()
    }

    pub(super) fn has_quote_reply(&self) -> bool {
        self.composer.quote.is_some()
            || self
                .composer
                .pending
                .as_ref()
                .is_some_and(|pending| pending.quote.is_some())
            || self
                .composer
                .awaiting_reconciliation
                .as_ref()
                .is_some_and(|reconciliation| reconciliation.quote.is_some())
    }

    /// Returns only display-safe affordance state for one typed reaction. The
    /// opaque Graph message ID stays encapsulated so the UI cannot manufacture
    /// a route from an index. Directory preview rows remain read-only until
    /// the selected `ChatMessages` history has replaced them.
    fn reaction_view(
        &self,
        message_index: usize,
        reaction: ReactionKind,
    ) -> Option<RemoteChatReactionView> {
        let metadata = self.message_reaction_metadata.get(message_index)?;
        let (count, selected) = metadata.reaction_state(reaction);
        let pending = metadata.target.as_ref().is_some_and(|target| {
            self.pending_reactions
                .iter()
                .any(|pending| pending.message == *target && pending.reaction == reaction)
        });
        Some(RemoteChatReactionView {
            reaction,
            count,
            selected,
            pending,
            can_react: matches!(self.history_state, RemoteChatHistoryState::Ready)
                && metadata.target.is_some()
                && !metadata.deleted
                && !metadata
                    .target
                    .as_ref()
                    .is_some_and(|target| self.is_being_mutated(target)),
        })
    }

    fn reaction_views(&self, message_index: usize) -> Option<Vec<RemoteChatReactionView>> {
        ReactionKind::ALL
            .into_iter()
            .map(|reaction| self.reaction_view(message_index, reaction))
            .collect()
    }

    /// Stages one supported set/unset request for a current selected-history
    /// row. Neither the visible count nor selection is updated here.
    fn queue_reaction(
        &mut self,
        message_index: usize,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<RemoteChatReactionRequest> {
        if !matches!(self.history_state, RemoteChatHistoryState::Ready) {
            return None;
        }
        let metadata = self.message_reaction_metadata.get(message_index)?;
        if metadata.deleted || metadata.reaction_state(reaction).1 == selected {
            return None;
        }
        let message = metadata.target.clone()?;
        if self.is_being_mutated(&message) {
            return None;
        }
        if self
            .pending_reactions
            .iter()
            .any(|pending| pending.message == message && pending.reaction == reaction)
        {
            return None;
        }
        self.pending_reactions.push(PendingRemoteChatReaction {
            message: message.clone(),
            reaction,
            selected,
            intent_id: None,
        });
        Some(RemoteChatReactionRequest {
            chat: self.target.clone(),
            message,
            reaction,
            selected,
        })
    }

    fn bind_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        message: &ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        let Some(pending) = self.pending_reactions.iter_mut().find(|pending| {
            pending.intent_id.is_none()
                && pending.message == *message
                && pending.reaction == reaction
        }) else {
            return false;
        };
        pending.intent_id = Some(intent_id);
        true
    }

    fn has_dispatchable_reaction(
        &self,
        message: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        matches!(self.history_state, RemoteChatHistoryState::Ready)
            && self.pending_reactions.iter().any(|pending| {
                pending.intent_id.is_none()
                    && pending.message == *message
                    && pending.reaction == reaction
                    && pending.selected == selected
            })
            && self.message_reaction_metadata.iter().any(|metadata| {
                metadata.target.as_ref() == Some(message)
                    && !metadata.deleted
                    && metadata.reaction_state(reaction).1 != selected
            })
    }

    fn has_reaction_intent(&self, intent_id: WorkspaceIntentId) -> bool {
        self.pending_reactions
            .iter()
            .any(|pending| pending.intent_id == Some(intent_id))
    }

    fn resolve_reaction_terminal_failure(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(index) = self
            .pending_reactions
            .iter()
            .position(|pending| pending.intent_id == Some(intent_id))
        else {
            return false;
        };
        self.pending_reactions.swap_remove(index);
        true
    }

    fn clear_unbound_pending_reactions(&mut self) {
        self.pending_reactions
            .retain(|pending| pending.intent_id.is_some());
    }

    /// A selected-history replacement owns the current set of usable message
    /// targets. A click that has not reached the controller cannot outlive a
    /// missing or deleted target, while a bound mutation remains a tombstone
    /// until a terminal outcome or matching provider state resolves it.
    fn clear_unbound_reactions_without_current_target(&mut self) {
        let metadata = &self.message_reaction_metadata;
        self.pending_reactions.retain(|pending| {
            pending.intent_id.is_some()
                || metadata.iter().any(|message| {
                    message.target.as_ref() == Some(&pending.message) && !message.deleted
                })
        });
    }

    /// Reduces an invisible conversation to the smallest state needed to
    /// route outcomes for already-controller-bound reaction requests. Unlike
    /// a normal bounded-directory omission, a worker loss has invalidated the
    /// whole projection and its send-recovery path, so neither rows nor
    /// composer state may survive this transition.
    fn retain_bound_reaction_tombstone(&mut self) -> bool {
        self.messages.clear();
        self.message_reaction_metadata.clear();
        self.rows_from_selected_history = false;
        self.history_mentions_current_user = false;
        self.history_newest_message = None;
        self.refresh_preview();
        self.rebuild_search_key();
        self.history_state = RemoteChatHistoryState::NotRequested;
        self.refresh_failed = false;
        self.pinned_messages = RemotePinnedMessagesState::Closed;
        self.members = RemoteChatMembersState::Closed;
        self.presence_due = None;
        self.composer = RemoteComposerState::default();
        self.clear_unbound_pending_reactions();
        !self.pending_reactions.is_empty()
    }

    /// Only the selected `ChatMessages` projection can reconcile a reaction,
    /// and only when the exact opaque message target and reaction kind carry
    /// the selected state requested by that mutation. A stale copy of the
    /// same message or another reaction kind remains locked.
    fn resolve_pending_reaction_projection(&mut self) {
        self.pending_reactions.retain(|pending| {
            !self.message_reaction_metadata.iter().any(|metadata| {
                metadata.target.as_ref() == Some(&pending.message)
                    && metadata.reaction_state(pending.reaction).1 == pending.selected
            })
        });
    }

    fn begin_history_load(&mut self) -> ProviderResourceRef {
        self.refresh_failed = false;
        self.history_state = RemoteChatHistoryState::Loading;
        self.target.clone()
    }

    fn apply_history(&mut self, projection: RemoteChatMessagesProjection) {
        debug_assert_eq!(self.target, projection.chat());
        if projection.is_unavailable() {
            // A transient failure must not leave the selected history stuck
            // on Loading. Keep whatever rows are visible and return to the
            // state from which the next directory refresh or re-selection
            // requests a fresh read; a completed Ready history stays Ready.
            match self.history_state {
                RemoteChatHistoryState::Loading => {
                    self.history_state = RemoteChatHistoryState::NotRequested;
                }
                RemoteChatHistoryState::Refreshing => {
                    // Keep the stale rows, never re-enable their controls.
                    self.history_state = RemoteChatHistoryState::Stale;
                    self.refresh_failed = true;
                }
                _ => {}
            }
            self.release_delivered_mutation();
            return;
        }
        if projection.is_access_denied() {
            // A target-bound 403 must not leave an earlier explicit history
            // visible as though it were freshly authorized. The chat-directory
            // preview will be restored only by a later valid snapshot.
            self.messages.clear();
            self.message_reaction_metadata.clear();
            self.rows_from_selected_history = false;
            self.history_mentions_current_user = false;
            self.history_newest_message = None;
            self.refresh_preview();
            self.rebuild_search_key();
            self.clear_unbound_reactions_without_current_target();
            self.clear_unbound_quote_reply();
            let _ = self.cancel_edit();
            self.release_delivered_mutation();
            self.refresh_failed = false;
            self.history_state = RemoteChatHistoryState::AccessDenied;
            return;
        }
        let (messages, message_reaction_metadata) =
            presentation_messages(projection.messages(), self.color);
        self.messages = messages;
        self.message_reaction_metadata = message_reaction_metadata;
        self.rows_from_selected_history = true;
        self.history_mentions_current_user = self.history_has_current_user_mention();
        self.history_newest_message = self.newest_message_target().cloned();
        self.refresh_preview();
        self.rebuild_search_key();
        self.history_state = RemoteChatHistoryState::Ready;
        self.refresh_failed = false;
        self.clear_unbound_reactions_without_current_target();
        self.clear_unbound_quote_reply_without_current_target();
        self.resolve_pending_reaction_projection();
        // A quote-reply ambiguity may be confirmed only by this exact
        // selected chat-history projection, never by a directory preview.
        self.observe_reconciliation_projection();
        // The same holds for an edit or deletion of an own message.
        let (messages, metadata) = (
            std::mem::take(&mut self.messages),
            std::mem::take(&mut self.message_reaction_metadata),
        );
        self.observe_mutation_rows(&messages, &metadata);
        self.messages = messages;
        self.message_reaction_metadata = metadata;
        self.cancel_edit_without_current_row();
    }

    fn history_state(&self) -> RemoteChatHistoryState {
        self.history_state
    }

    /// `query` must already be lowercase. The key joins fields with `\n`, so
    /// a single-line query cannot match across two fields.
    pub(super) fn matches_query(&self, query: &str) -> bool {
        self.search_key.contains(query)
    }

    pub(super) fn last_preview(&self) -> &str {
        &self.preview_text
    }

    pub(super) fn last_time(&self) -> &str {
        &self.preview_time
    }

    pub(super) const fn kind(&self) -> RemoteChatKind {
        self.kind
    }

    pub(super) fn draft_mut(&mut self) -> &mut String {
        &mut self.composer.draft
    }

    /// Whether the composer may take an `@` mention now: an ordinary send,
    /// not a quote reply (plain text), an edit, or a locked composer.
    pub(super) fn can_mention(&self) -> bool {
        !self.has_pending_send() && self.composer.quote.is_none() && !self.is_editing()
    }

    /// The members the `@` picker may offer; empty until a People roster has
    /// been read for this chat.
    pub(super) fn mention_roster(&self) -> &[DraftMention] {
        &self.mention_roster
    }

    /// Remembers a picked member for the next send. The same member is kept
    /// once, and no more than [`MAX_DRAFT_MENTIONS`] are held; returns whether
    /// the pick can be used.
    pub(super) fn add_draft_mention(&mut self, member: DraftMention) -> bool {
        if self
            .composer
            .draft_mentions
            .iter()
            .any(|known| known.handle == member.handle)
        {
            return true;
        }
        if self.composer.draft_mentions.len() >= MAX_DRAFT_MENTIONS {
            return false;
        }
        self.composer.draft_mentions.push(member);
        true
    }

    /// The priority visible in the remote-only composer. While the send is
    /// pending or under reconciliation, surface the captured value but keep
    /// the selector disabled; a confirmed send then returns the fresh
    /// composer to its Normal default.
    pub(super) const fn composer_importance(&self) -> RemoteChatMessageImportance {
        if let Some(pending) = &self.composer.pending {
            pending.importance
        } else if let Some(reconciliation) = &self.composer.awaiting_reconciliation {
            reconciliation.importance
        } else {
            self.composer.importance
        }
    }

    /// Changes a currently editable remote composer priority. Pending and
    /// reconciliation states deliberately ignore updates, so the request and
    /// displayed disabled selector cannot diverge.
    pub(super) fn set_composer_importance(&mut self, importance: RemoteChatMessageImportance) {
        if !self.has_pending_send() && self.composer.quote.is_none() && !self.is_editing() {
            self.composer.importance = importance;
        }
    }

    /// Whether the composer is locked by a send, a reconciliation, or an
    /// edit or deletion of an own message.
    pub(super) const fn has_pending_send(&self) -> bool {
        self.composer.pending.is_some()
            || self.composer.awaiting_reconciliation.is_some()
            || self.mutation.is_some()
    }

    pub(super) const fn is_awaiting_reconciliation(&self) -> bool {
        self.composer.awaiting_reconciliation.is_some()
    }

    pub(super) fn has_draft(&self) -> bool {
        !self.composer.draft.trim().is_empty()
            || self.composer.awaiting_reconciliation.is_some()
            || self.composer.attachment.is_some()
    }

    fn queue_message(
        &mut self,
        submission: RemoteSubmissionId,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.has_pending_send() {
            return Err(RemoteSendError::Pending);
        }
        if self
            .composer
            .quote
            .as_ref()
            .is_some_and(|selection| !self.has_current_quote_sources(selection))
        {
            // A bound request can fail after its source was deleted or the
            // selected history changed. Do not leave its recovered composer
            // locked behind a source target that no longer exists.
            let _ = self.cancel_quote_reply();
            return Err(RemoteSendError::QuoteUnavailable);
        }
        // The file, when there is one, is the message: the text is optional.
        // A quote reply is plain text and carries no file.
        let attachment = self.sendable_attachment()?;
        if attachment.is_some() && self.composer.quote.is_some() {
            return Err(RemoteSendError::AttachmentBlockedByMode);
        }
        let draft_source = self.composer.draft.trim().to_owned();
        if draft_source.is_empty() && attachment.is_none() {
            return Err(RemoteSendError::Empty);
        }
        if draft_source.len() > MAX_REMOTE_MESSAGE_TEXT_BYTES {
            return Err(RemoteSendError::TooLong);
        }
        // Quote replies are plain text on the wire, so their draft is taken
        // literally; an ordinary send interprets markdown-lite.
        // Only mentions whose `@Name` is still in the draft can apply; a quote
        // reply has no mentions at all.
        self.composer
            .draft_mentions
            .retain(|member| draft_source.contains(&format!("@{}", member.display)));
        let draft_mentions = std::mem::take(&mut self.composer.draft_mentions);
        let body = if self.composer.quote.is_some() {
            ComposedBody::plain(&draft_source)
        } else {
            ComposedBody::compose(&draft_source, &draft_mentions)
        };
        // A newly editable composer defaults back to Normal after the send
        // leaves it. Quote mode has already forced that value to Normal and
        // retains the earlier selector value inside its selection for an
        // explicit cancel; ordinary sends retain their chosen priority here.
        let importance = std::mem::take(&mut self.composer.importance);
        let quote = self.composer.quote.take();
        let quoted_messages = quote.as_ref().map(RemoteQuoteSelection::targets);
        self.composer.draft.clear();
        // The file leaves the composer with the draft and returns with it.
        if attachment.is_some() {
            self.composer.attachment = None;
        }
        self.composer.pending = Some(PendingRemoteSend {
            submission,
            body: body.clone(),
            draft_source,
            draft_mentions,
            importance,
            quote,
            attachment: attachment.clone(),
            intent_id: None,
            cancellation_requested: false,
        });
        Ok(match quoted_messages {
            Some(quoted_messages) => RemoteSendRequest::QuoteReply {
                submission,
                target: self.target.clone(),
                quoted_messages,
                body: body.plain_projection(),
            },
            None => RemoteSendRequest::Message {
                submission,
                target: self.target.clone(),
                body,
                importance,
                attachment,
            },
        })
    }

    fn bind_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        let Some(pending) = self.composer.pending.as_mut() else {
            return false;
        };
        if pending.submission != submission {
            return false;
        }
        pending.intent_id = Some(intent_id);
        true
    }

    fn has_intent(&self, intent_id: WorkspaceIntentId) -> bool {
        self.composer
            .pending
            .as_ref()
            .is_some_and(|pending| pending.intent_id == Some(intent_id))
    }

    fn has_submission(&self, submission: RemoteSubmissionId) -> bool {
        self.composer
            .pending
            .as_ref()
            .is_some_and(|pending| pending.submission == submission)
    }

    fn has_dispatchable_quote_submission(
        &self,
        submission: RemoteSubmissionId,
        messages: &[ProviderResourceRef],
    ) -> bool {
        matches!(self.history_state, RemoteChatHistoryState::Ready)
            && self.composer.pending.as_ref().is_some_and(|pending| {
                pending.submission == submission
                    && pending.intent_id.is_none()
                    && pending.quote.as_ref().is_some_and(|selection| {
                        selection.targets() == messages && self.has_current_quote_sources(selection)
                    })
            })
    }

    fn has_current_quote_sources(&self, selection: &RemoteQuoteSelection) -> bool {
        selection
            .sources
            .iter()
            .all(|source| self.has_current_quote_source(&source.target))
    }

    fn has_current_quote_source(&self, message: &ProviderResourceRef) -> bool {
        matches!(self.history_state, RemoteChatHistoryState::Ready)
            && self
                .message_reaction_metadata
                .iter()
                .any(|metadata| !metadata.deleted && metadata.target.as_ref() == Some(message))
    }

    fn restore_pending_to_composer(&mut self, pending: PendingRemoteSend) {
        if pending
            .quote
            .as_ref()
            .is_some_and(|selection| !self.has_current_quote_sources(selection))
        {
            self.restore_unbound_pending_to_plain_draft(pending);
            return;
        }
        self.composer.draft = pending.draft_source;
        self.composer.draft_mentions = pending.draft_mentions;
        self.composer.importance = pending.importance;
        self.composer.quote = pending.quote;
        self.restore_attachment(pending.attachment);
    }

    fn restore_reconciliation_to_composer(&mut self, reconciliation: AwaitingRemoteReconciliation) {
        if reconciliation
            .quote
            .as_ref()
            .is_some_and(|selection| !self.has_current_quote_sources(selection))
        {
            self.composer.quote_notice = Some(QUOTE_SOURCES_GONE_NOTICE);
            self.composer.draft = reconciliation.draft_source;
            self.composer.draft_mentions = reconciliation.draft_mentions;
            self.restore_attachment(reconciliation.attachment);
            self.composer.importance = reconciliation
                .quote
                .map_or(reconciliation.importance, |selection| {
                    selection.priority_before_quote
                });
            self.composer.quote = None;
            return;
        }
        self.composer.draft = reconciliation.draft_source;
        self.composer.draft_mentions = reconciliation.draft_mentions;
        self.composer.importance = reconciliation.importance;
        self.composer.quote = reconciliation.quote;
        self.restore_attachment(reconciliation.attachment);
    }

    /// Restores an unbound quote send as an ordinary draft when the source
    /// history disappears. The opaque source target cannot outlive the row
    /// that supplied it, while the user's body and pre-quote priority remain
    /// available for deliberate editing.
    fn restore_unbound_pending_to_plain_draft(&mut self, pending: PendingRemoteSend) {
        if pending.quote.is_some() {
            self.composer.quote_notice = Some(QUOTE_SOURCES_GONE_NOTICE);
        }
        self.composer.draft = pending.draft_source;
        self.composer.draft_mentions = pending.draft_mentions;
        self.restore_attachment(pending.attachment);
        self.composer.importance = pending.quote.map_or(pending.importance, |selection| {
            selection.priority_before_quote
        });
        self.composer.quote = None;
    }

    /// Returns a send's file to the composer, so a failed send is retried
    /// with the very same upload rather than a second one.
    fn restore_attachment(&mut self, attachment: Option<UploadedFileHandle>) {
        if let Some(handle) = attachment {
            self.composer.attachment = Some(ComposerAttachment::Ready { handle });
        }
    }

    fn clear_unbound_quote_reply(&mut self) {
        if self
            .composer
            .pending
            .as_ref()
            .is_some_and(|pending| pending.intent_id.is_none() && pending.quote.is_some())
            && let Some(pending) = self.composer.pending.take()
        {
            self.restore_unbound_pending_to_plain_draft(pending);
        }
        if self.composer.pending.is_none() && self.composer.quote.is_some() {
            let _ = self.cancel_quote_reply();
        }
    }

    fn discard_unbound_quote_reply_when_history_unavailable(&mut self) {
        if !matches!(self.history_state, RemoteChatHistoryState::Ready) {
            self.clear_unbound_quote_reply();
            let _ = self.cancel_edit();
        }
    }

    fn clear_unbound_quote_reply_without_current_target(&mut self) {
        // An unsent selection drops only the sources that vanished; with none
        // left it becomes a plain draft again.
        let vanished: Vec<usize> = self
            .composer
            .quote
            .as_ref()
            .map(|selection| {
                selection
                    .sources
                    .iter()
                    .enumerate()
                    .filter(|(_, source)| !self.has_current_quote_source(&source.target))
                    .map(|(index, _)| index)
                    .collect()
            })
            .unwrap_or_default();
        if !vanished.is_empty() && !self.has_pending_send() {
            for index in vanished.into_iter().rev() {
                let _ = self.remove_quote_source(index);
            }
            self.composer.quote_notice = Some(QUOTE_SOURCES_GONE_NOTICE);
        }
        // A pending set is never shrunk: if any source vanished before the
        // command was bound, it returns to a plain draft.
        let pending_vanished = self.composer.pending.as_ref().is_some_and(|pending| {
            pending.intent_id.is_none()
                && pending
                    .quote
                    .as_ref()
                    .is_some_and(|selection| !self.has_current_quote_sources(selection))
        });
        if pending_vanished && let Some(pending) = self.composer.pending.take() {
            self.restore_unbound_pending_to_plain_draft(pending);
        }
    }

    fn resolve_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        if !self.has_intent(intent_id) {
            return false;
        }
        // Do not optimistically append a Message. Only the next normalized
        // provider projection can show the actual remote content.
        self.composer.pending = None;
        true
    }

    fn resolve_terminal_failure(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(pending) = self.composer.pending.take() else {
            return false;
        };
        if pending.intent_id != Some(intent_id) {
            self.composer.pending = Some(pending);
            return false;
        }
        // The field is locked while pending, so restoring the exact body,
        // quote selection, and priority makes a provider-confirmed rejection
        // reviewable and retryable.
        self.restore_pending_to_composer(pending);
        true
    }

    fn resolve_delivery_uncertain(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(pending) = self.composer.pending.take() else {
            return false;
        };
        if pending.intent_id != Some(intent_id) {
            self.composer.pending = Some(pending);
            return false;
        }
        let quoted_messages = pending
            .quote
            .as_ref()
            .map(RemoteQuoteSelection::targets)
            .unwrap_or_default();
        let baseline_had_matching_own_message = self.has_matching_own_message(
            &pending.body,
            pending.importance,
            &quoted_messages,
            pending
                .attachment
                .as_ref()
                .map(UploadedFileHandle::reference_id),
        );
        self.composer.awaiting_reconciliation = Some(AwaitingRemoteReconciliation {
            intent_id,
            body: pending.body,
            draft_source: pending.draft_source,
            draft_mentions: pending.draft_mentions,
            importance: pending.importance,
            quote: pending.quote,
            attachment: pending.attachment,
            baseline_had_matching_own_message,
            confirmed_in_projection: false,
        });
        true
    }

    /// A same-text own message only confirms an ambiguous send when its
    /// normalized priority matches too. Quote replies additionally require
    /// the exact opaque `messageReference` source target; body text alone is
    /// never enough to confirm a non-idempotent reply-with-quote POST.
    fn has_matching_own_message(
        &self,
        body: &ComposedBody,
        importance: RemoteChatMessageImportance,
        quoted_messages: &[ProviderResourceRef],
        attachment: Option<&str>,
    ) -> bool {
        rows_have_matching_own_message(
            &self.messages,
            &self.message_reaction_metadata,
            body,
            importance,
            quoted_messages,
            attachment,
        )
    }

    fn reconciliation_probe(&self) -> Option<ReconciliationProbe> {
        self.composer
            .awaiting_reconciliation
            .as_ref()
            .map(|reconciliation| ReconciliationProbe {
                baseline_had_matching_own_message: reconciliation.baseline_had_matching_own_message,
                body: reconciliation.body.clone(),
                importance: reconciliation.importance,
                quoted_messages: reconciliation
                    .quote
                    .as_ref()
                    .map(RemoteQuoteSelection::targets)
                    .unwrap_or_default(),
                attachment: reconciliation
                    .attachment
                    .as_ref()
                    .map(|handle| handle.reference_id().to_owned()),
            })
    }

    /// Confirms an ambiguous send from this conversation's own just-applied
    /// selected history. A directory preview is never passed here.
    fn observe_reconciliation_projection(&mut self) {
        let Some(probe) = self.reconciliation_probe() else {
            return;
        };
        if !probe.baseline_had_matching_own_message
            && self.has_matching_own_message(
                &probe.body,
                probe.importance,
                &probe.quoted_messages,
                probe.attachment.as_deref(),
            )
            && let Some(reconciliation) = self.composer.awaiting_reconciliation.as_mut()
        {
            reconciliation.confirmed_in_projection = true;
        }
    }

    /// Observes an exact selected-history projection for this chat without
    /// applying its rows, so a result that arrives after navigation can still
    /// settle an ambiguous send instead of restoring a draft that Graph has
    /// already accepted.
    fn observe_reconciliation_history(&mut self, projection: &RemoteChatMessagesProjection) {
        debug_assert_eq!(self.target, projection.chat());
        if projection.is_unavailable() || projection.is_access_denied() {
            self.release_delivered_mutation();
            return;
        }
        if self.mutation.is_some() {
            let (messages, metadata) = presentation_messages(projection.messages(), self.color);
            self.observe_mutation_rows(&messages, &metadata);
        }
        let Some(probe) = self.reconciliation_probe() else {
            return;
        };
        if probe.baseline_had_matching_own_message {
            return;
        }
        let (messages, metadata) = presentation_messages(projection.messages(), self.color);
        if rows_have_matching_own_message(
            &messages,
            &metadata,
            &probe.body,
            probe.importance,
            &probe.quoted_messages,
            probe.attachment.as_deref(),
        ) && let Some(reconciliation) = self.composer.awaiting_reconciliation.as_mut()
        {
            reconciliation.confirmed_in_projection = true;
        }
    }

    fn complete_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteReconciliationResolution> {
        let reconciliation = self.composer.awaiting_reconciliation.take()?;
        if reconciliation.intent_id != intent_id {
            self.composer.awaiting_reconciliation = Some(reconciliation);
            return None;
        }
        if reconciliation.confirmed_in_projection {
            return Some(RemoteReconciliationResolution::Confirmed);
        }
        self.restore_reconciliation_to_composer(reconciliation);
        Some(RemoteReconciliationResolution::DraftRestored)
    }

    fn fail_reconciliation(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(reconciliation) = self.composer.awaiting_reconciliation.take() else {
            return false;
        };
        if reconciliation.intent_id != intent_id {
            self.composer.awaiting_reconciliation = Some(reconciliation);
            return false;
        }
        self.restore_reconciliation_to_composer(reconciliation);
        true
    }

    fn restore_unbound_pending_to_draft(&mut self) {
        self.restore_unbound_mutation();
        if self
            .composer
            .pending
            .as_ref()
            .is_some_and(|pending| pending.intent_id.is_none())
            && let Some(pending) = self.composer.pending.take()
        {
            self.restore_unbound_pending_to_plain_draft(pending);
        }
    }

    fn request_unstarted_cancellation(&mut self) -> Option<WorkspaceIntentId> {
        if let Some(intent_id) = self.request_mutation_cancellation() {
            return Some(intent_id);
        }
        let pending = self.composer.pending.as_mut()?;
        let intent_id = pending.intent_id?;
        if pending.cancellation_requested {
            return None;
        }
        pending.cancellation_requested = true;
        Some(intent_id)
    }

    /// Whether the only reason this conversation is retained is an unsent
    /// draft or quote selection (no send, reconciliation, or reaction gate).
    fn retains_only_a_draft(&self) -> bool {
        self.composer.pending.is_none()
            && self.composer.awaiting_reconciliation.is_none()
            && !matches!(
                self.composer.attachment,
                Some(ComposerAttachment::Uploading { .. })
            )
            && self.mutation.is_none()
            && self.pending_reactions.is_empty()
    }

    fn retains_session_state(&self) -> bool {
        !self.composer.draft.trim().is_empty()
            || self.composer.pending.is_some()
            || self.composer.awaiting_reconciliation.is_some()
            || self.composer.attachment.is_some()
            || self.mutation.is_some()
            || !self.pending_reactions.is_empty()
    }

    #[cfg(test)]
    pub(super) fn draft(&self) -> &str {
        &self.composer.draft
    }
}

pub(super) fn draw_remote_sidebar_row(
    ui: &mut egui::Ui,
    row: &RemoteConversation,
    selected: bool,
) -> bool {
    let preview = format!(
        "M365 · {} · {}",
        remote_kind_label(row.kind()),
        row.last_preview()
    );
    ListRow::new(row.title())
        .timestamp(row.last_time())
        .preview(&preview)
        .selected(selected)
        .unread(row.is_unread())
        .badge(row.new_count())
        .avatar(Avatar::new(row.title(), AvatarSize::Md32))
        .show(ui)
        .clicked()
}

fn remote_subtitle(kind: RemoteChatKind, subtitle: &str) -> String {
    let subtitle = subtitle.trim();
    if subtitle.is_empty() {
        format!("Microsoft 365 · {}", remote_kind_label(kind))
    } else {
        format!("Microsoft 365 · {} · {subtitle}", remote_kind_label(kind))
    }
}

fn remote_kind_label(kind: RemoteChatKind) -> &'static str {
    match kind {
        RemoteChatKind::OneOnOne => "1:1 chat",
        RemoteChatKind::Group => "group chat",
        RemoteChatKind::Meeting => "meeting chat",
        RemoteChatKind::Unknown => "chat",
    }
}

fn nonempty_or(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.into()
    } else {
        value.into()
    }
}

fn remote_initials(initials: &str, title: &str) -> String {
    let initials = initials.trim();
    if !initials.is_empty() {
        return initials.chars().take(3).collect();
    }
    title
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
        .chars()
        .take(3)
        .collect()
}

fn remote_color(title: &str) -> Color32 {
    let hash = title.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(1099511628211)
            .wrapping_add(u64::from(byte))
    });
    let accents = crate::ui::theme::remote_conversation_accents();
    accents[(hash as usize) % accents.len()]
}
