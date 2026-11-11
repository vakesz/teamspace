//! Edit and delete of the signed-in user's own Microsoft 365 chat messages.
//!
//! Both are session-only gates on one conversation. There is no optimistic
//! row: the provider-owned history stays as read. A `204` is delivery, but the
//! gate stays until the next exact selected-history read; an ambiguous
//! outcome is settled only by that exact chat's fresh history, and a missing
//! row is never proof of anything. Nothing here is persisted or replayed.

use super::{
    MAX_REMOTE_MESSAGE_TEXT_BYTES, RemoteChatHistoryState, RemoteChatState, RemoteConversation,
    RemoteMessageReactionMetadata, RemoteSendError, RemoteSendRequest, RemoteSubmissionId,
};
use crate::integrations::ComposedBody;
use crate::integrations::{ProviderResourceRef, RemoteCapabilities, WorkspaceIntentId};
use crate::shared::model::Message;

/// What an in-flight or unresolved mutation does to its message.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MutationKind {
    Edit {
        /// The visible text, for reconciliation against a fresh read.
        body: String,
        /// The composer text to restore if the edit is not confirmed.
        draft_source: String,
        original: ComposedBody,
    },
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationPhase {
    /// Queued or sent; no outcome yet.
    Pending,
    /// Graph returned `204`. The row is still the pre-mutation one until the
    /// next exact history read, which releases the gate.
    Delivered,
    /// The write may or may not have reached Graph. Only the exact chat's
    /// fresh history can show whether it did.
    AwaitingReconciliation { confirmed_in_projection: bool },
}

/// The one mutation a conversation may have in flight.
pub(in crate::features::chat) struct PendingRemoteMessageMutation {
    submission: RemoteSubmissionId,
    message: ProviderResourceRef,
    kind: MutationKind,
    intent_id: Option<WorkspaceIntentId>,
    cancellation_requested: bool,
    phase: MutationPhase,
}

/// The composer is editing one specific own message.
pub(in crate::features::chat) struct RemoteEditSelection {
    target: ProviderResourceRef,
    /// The message as it was, to tell an unchanged save from a real edit.
    original_body: ComposedBody,
    /// The markdown-lite text the composer was filled with.
    original_source: String,
}

/// Which action a mutation outcome concerns, for user-facing notices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteMutationLabel {
    Edit,
    Delete,
}

/// How an ambiguous mutation was settled by the exact chat's fresh history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteMutationResolution {
    pub(crate) label: RemoteMutationLabel,
    pub(crate) confirmed: bool,
}

/// Display-safe availability of the per-row Edit and Delete controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::features::chat) struct RemoteMessageActionsView {
    pub(in crate::features::chat) show_edit: bool,
    pub(in crate::features::chat) edit_enabled: bool,
    pub(in crate::features::chat) show_delete: bool,
}

/// Text equality that ignores layout: Graph returns an edited body as HTML,
/// so line breaks and runs of spaces collapse before the projection is read.
pub(super) fn same_visible_text(left: &str, right: &str) -> bool {
    left.split_whitespace().eq(right.split_whitespace())
}

impl RemoteConversation {
    pub(in crate::features::chat) const fn is_editing(&self) -> bool {
        self.composer.editing.is_some()
    }

    /// A headline for the composer while a mutation owns it.
    pub(in crate::features::chat) fn mutation_status(&self) -> Option<&'static str> {
        let mutation = self.mutation.as_ref()?;
        Some(match (&mutation.kind, mutation.phase) {
            (_, MutationPhase::AwaitingReconciliation { .. }) => {
                "Checking Microsoft 365 before allowing another change…"
            }
            (MutationKind::Edit { .. }, _) => "Saving your edit to Microsoft 365…",
            (MutationKind::Delete, _) => "Deleting the message in Microsoft 365…",
        })
    }

    pub(in crate::features::chat) fn editing_original_text(&self) -> Option<&str> {
        self.composer
            .editing
            .as_ref()
            .map(|selection| selection.original_source.as_str())
    }

    /// Whether an edit or deletion owns this message, so a reaction on it
    /// would race the write.
    pub(super) fn is_being_mutated(&self, message: &ProviderResourceRef) -> bool {
        self.mutation
            .as_ref()
            .is_some_and(|mutation| mutation.message == *message)
            || self
                .composer
                .editing
                .as_ref()
                .is_some_and(|selection| selection.target == *message)
    }

    /// The row index of an own, live, addressable message in a Ready history.
    fn own_row(&self, message: &ProviderResourceRef) -> Option<usize> {
        if !matches!(self.history_state, RemoteChatHistoryState::Ready) {
            return None;
        }
        self.message_reaction_metadata.iter().position(|metadata| {
            metadata.own && !metadata.deleted && metadata.target.as_ref() == Some(message)
        })
    }

    fn editable_row(&self, message: &ProviderResourceRef) -> Option<usize> {
        self.own_row(message)
            .filter(|index| self.message_reaction_metadata[*index].plain_text_editable)
    }

    /// Whether any gate on this conversation forbids starting a mutation.
    fn mutation_gated(&self) -> bool {
        self.has_pending_send() || self.composer.quote.is_some() || self.composer.editing.is_some()
    }

    pub(in crate::features::chat) fn message_actions_view(
        &self,
        message_index: usize,
        capabilities: RemoteCapabilities,
    ) -> Option<RemoteMessageActionsView> {
        let metadata = self.message_reaction_metadata.get(message_index)?;
        let target = metadata.target.as_ref()?;
        if !matches!(self.history_state, RemoteChatHistoryState::Ready)
            || !metadata.own
            || metadata.deleted
            || self.mutation_gated()
            || self
                .pending_reactions
                .iter()
                .any(|pending| &pending.message == target)
        {
            return None;
        }
        let show_edit = capabilities.chat_message_edit.is_offered() && metadata.plain_text_editable;
        let show_delete = capabilities.chat_message_delete.is_offered();
        (show_edit || show_delete).then(|| RemoteMessageActionsView {
            show_edit,
            // Edit reuses the composer, so an unsent draft must not be lost.
            edit_enabled: show_edit
                && self.composer.draft.trim().is_empty()
                && self.composer.quote.is_none(),
            show_delete,
        })
    }

    /// Enters edit mode for one own, plain-text row of the current history.
    pub(in crate::features::chat) fn begin_edit(
        &mut self,
        message_index: usize,
        capabilities: RemoteCapabilities,
    ) -> Result<(), RemoteSendError> {
        // Quote mode and edit mode refuse each other with an explicit notice.
        if self.composer.quote.is_some() && !self.has_pending_send() {
            return Err(RemoteSendError::EditBlockedByQuote);
        }
        if self.mutation_gated() {
            return Err(RemoteSendError::Pending);
        }
        if self.has_attachment() && !self.has_pending_send() {
            return Err(RemoteSendError::AttachmentBlockedByMode);
        }
        let view = self
            .message_actions_view(message_index, capabilities)
            .filter(|view| view.show_edit)
            .ok_or(RemoteSendError::EditUnavailable)?;
        if !view.edit_enabled {
            return Err(RemoteSendError::EditNeedsEmptyDraft);
        }
        let target = self.message_reaction_metadata[message_index]
            .target
            .clone()
            .ok_or(RemoteSendError::EditUnavailable)?;
        let message = self
            .messages
            .get(message_index)
            .ok_or(RemoteSendError::EditUnavailable)?;
        let original_body = match &message.rich {
            Some(runs) => ComposedBody::from_runs(runs),
            None => Some(ComposedBody::plain(&message.text)),
        }
        .ok_or(RemoteSendError::EditFormatting)?;
        // Markdown-lite has no escape syntax, so formatting that would not
        // read back identically is withheld rather than silently rewritten.
        let original_source = original_body.to_markdown_lite();
        if ComposedBody::from_markdown_lite(&original_source) != original_body {
            return Err(RemoteSendError::EditFormatting);
        }
        self.composer.draft.clone_from(&original_source);
        self.composer.editing = Some(RemoteEditSelection {
            target,
            original_body,
            original_source,
        });
        Ok(())
    }

    /// Leaves edit mode and discards the edit text (the draft was empty
    /// before it began).
    pub(in crate::features::chat) fn cancel_edit(&mut self) -> bool {
        if self.composer.editing.take().is_none() {
            return false;
        }
        self.composer.draft.clear();
        true
    }

    /// Edit mode survives only while its row is still an own, editable row of
    /// a Ready history.
    pub(in crate::features::chat) fn cancel_edit_without_current_row(&mut self) {
        let stale = self
            .composer
            .editing
            .as_ref()
            .is_some_and(|selection| self.editable_row(&selection.target).is_none());
        if stale {
            let _ = self.cancel_edit();
        }
    }

    /// Turns the edit composer into a pending edit request.
    pub(in crate::features::chat) fn queue_edit(
        &mut self,
        submission: RemoteSubmissionId,
        capabilities: RemoteCapabilities,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.has_pending_send() {
            return Err(RemoteSendError::Pending);
        }
        let Some(selection) = self.composer.editing.as_ref() else {
            return Err(RemoteSendError::EditUnavailable);
        };
        if !capabilities.chat_message_edit.is_offered()
            || self.editable_row(&selection.target).is_none()
        {
            let _ = self.cancel_edit();
            return Err(RemoteSendError::EditUnavailable);
        }
        let draft_source = self.composer.draft.trim().to_owned();
        if draft_source.is_empty() {
            return Err(RemoteSendError::Empty);
        }
        if draft_source.len() > MAX_REMOTE_MESSAGE_TEXT_BYTES {
            return Err(RemoteSendError::TooLong);
        }
        let body = ComposedBody::from_markdown_lite(&draft_source);
        // A formatting-only change counts; whitespace alone does not.
        if body == selection.original_body
            || (body.is_plain()
                && selection.original_body.is_plain()
                && same_visible_text(
                    &body.plain_projection(),
                    &selection.original_body.plain_projection(),
                ))
        {
            return Err(RemoteSendError::Unchanged);
        }
        let selection = self
            .composer
            .editing
            .take()
            .ok_or(RemoteSendError::EditUnavailable)?;
        self.composer.draft.clear();
        self.mutation = Some(PendingRemoteMessageMutation {
            submission,
            message: selection.target.clone(),
            kind: MutationKind::Edit {
                body: body.plain_projection(),
                draft_source,
                original: selection.original_body,
            },
            intent_id: None,
            cancellation_requested: false,
            phase: MutationPhase::Pending,
        });
        Ok(RemoteSendRequest::Edit {
            submission,
            target: self.target.clone(),
            message: selection.target,
            body,
        })
    }

    pub(in crate::features::chat) fn queue_delete(
        &mut self,
        submission: RemoteSubmissionId,
        message: &ProviderResourceRef,
        capabilities: RemoteCapabilities,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.mutation_gated() {
            return Err(RemoteSendError::Pending);
        }
        if !capabilities.chat_message_delete.is_offered() || self.own_row(message).is_none() {
            return Err(RemoteSendError::DeleteUnavailable);
        }
        self.mutation = Some(PendingRemoteMessageMutation {
            submission,
            message: message.clone(),
            kind: MutationKind::Delete,
            intent_id: None,
            cancellation_requested: false,
            phase: MutationPhase::Pending,
        });
        Ok(RemoteSendRequest::Delete {
            submission,
            target: self.target.clone(),
            message: message.clone(),
        })
    }

    pub(in crate::features::chat) fn has_mutation_submission(
        &self,
        submission: RemoteSubmissionId,
    ) -> bool {
        self.mutation
            .as_ref()
            .is_some_and(|mutation| mutation.submission == submission)
    }

    pub(in crate::features::chat) fn bind_mutation_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        match self.mutation.as_mut() {
            Some(mutation) if mutation.submission == submission => {
                mutation.intent_id = Some(intent_id);
                true
            }
            _ => false,
        }
    }

    pub(in crate::features::chat) fn has_mutation_intent(
        &self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.mutation
            .as_ref()
            .is_some_and(|mutation| mutation.intent_id == Some(intent_id))
    }

    /// An unbound request stays dispatchable only while its exact row is
    /// still a current own row of the Ready history.
    pub(in crate::features::chat) fn has_dispatchable_mutation(
        &self,
        submission: RemoteSubmissionId,
        message: &ProviderResourceRef,
    ) -> bool {
        self.mutation.as_ref().is_some_and(|mutation| {
            mutation.submission == submission
                && mutation.intent_id.is_none()
                && mutation.message == *message
                && match mutation.kind {
                    MutationKind::Edit { .. } => self.editable_row(message).is_some(),
                    MutationKind::Delete => self.own_row(message).is_some(),
                }
        })
    }

    /// Releases a mutation that never reached the controller and restores
    /// what the user typed.
    pub(in crate::features::chat) fn restore_unbound_mutation(&mut self) {
        if self
            .mutation
            .as_ref()
            .is_some_and(|mutation| mutation.intent_id.is_none())
            && let Some(mutation) = self.mutation.take()
        {
            self.restore_after_mutation(mutation);
        }
    }

    pub(in crate::features::chat) fn request_mutation_cancellation(
        &mut self,
    ) -> Option<WorkspaceIntentId> {
        let mutation = self.mutation.as_mut()?;
        let intent_id = mutation.intent_id?;
        if mutation.cancellation_requested || mutation.phase != MutationPhase::Pending {
            return None;
        }
        mutation.cancellation_requested = true;
        Some(intent_id)
    }

    /// `204`: delivered, but the gate stays until the exact history read.
    pub(in crate::features::chat) fn resolve_mutation_delivered(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        match self.mutation.as_mut() {
            Some(mutation) if mutation.intent_id == Some(intent_id) => {
                mutation.phase = MutationPhase::Delivered;
                true
            }
            _ => false,
        }
    }

    /// A known rejection (or a cancellation before send) unlocks the gate.
    pub(in crate::features::chat) fn resolve_mutation_rejected(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationLabel> {
        if !self.has_mutation_intent(intent_id) {
            return None;
        }
        let mutation = self.mutation.take()?;
        let label = mutation.label();
        self.restore_after_mutation(mutation);
        Some(label)
    }

    pub(in crate::features::chat) fn resolve_mutation_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        match self.mutation.as_mut() {
            Some(mutation) if mutation.intent_id == Some(intent_id) => {
                mutation.phase = MutationPhase::AwaitingReconciliation {
                    confirmed_in_projection: false,
                };
                true
            }
            _ => false,
        }
    }

    /// Reads an exact selected-history result for this chat. Only these rows
    /// can confirm an ambiguous mutation, and a missing row confirms nothing.
    pub(super) fn observe_mutation_rows(
        &mut self,
        messages: &[Message],
        metadata: &[RemoteMessageReactionMetadata],
    ) {
        let Some(phase) = self.mutation.as_ref().map(|mutation| mutation.phase) else {
            return;
        };
        match phase {
            MutationPhase::Pending => {}
            MutationPhase::Delivered => {
                // The provider read after the `204` has arrived.
                self.mutation = None;
            }
            MutationPhase::AwaitingReconciliation {
                confirmed_in_projection: true,
            } => {}
            MutationPhase::AwaitingReconciliation {
                confirmed_in_projection: false,
            } => {
                let Some(mutation) = self.mutation.as_mut() else {
                    return;
                };
                let confirmed = messages.iter().zip(metadata).any(|(message, row)| {
                    if row.target.as_ref() != Some(&mutation.message) {
                        return false;
                    }
                    match &mutation.kind {
                        MutationKind::Edit { body, .. } => {
                            row.own && !row.deleted && same_visible_text(&message.text, body)
                        }
                        MutationKind::Delete => row.deleted,
                    }
                });
                if confirmed {
                    mutation.phase = MutationPhase::AwaitingReconciliation {
                        confirmed_in_projection: true,
                    };
                }
            }
        }
    }

    /// An unavailable or access-denied read still ends the post-`204` wait,
    /// so the controls cannot stay locked behind a failed refresh.
    pub(in crate::features::chat) fn release_delivered_mutation(&mut self) {
        if self
            .mutation
            .as_ref()
            .is_some_and(|mutation| mutation.phase == MutationPhase::Delivered)
        {
            self.mutation = None;
        }
    }

    pub(in crate::features::chat) fn complete_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationResolution> {
        if !self.has_mutation_intent(intent_id) {
            return None;
        }
        let mutation = self.mutation.take()?;
        let label = mutation.label();
        let confirmed = matches!(
            mutation.phase,
            MutationPhase::AwaitingReconciliation {
                confirmed_in_projection: true
            }
        );
        if !confirmed {
            self.restore_after_mutation(mutation);
        }
        Some(RemoteMutationResolution { label, confirmed })
    }

    pub(in crate::features::chat) fn fail_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationLabel> {
        if !self.has_mutation_intent(intent_id) {
            return None;
        }
        let mutation = self.mutation.take()?;
        let label = mutation.label();
        self.restore_after_mutation(mutation);
        Some(label)
    }

    /// Puts an unconfirmed edit back where the user can review it: into edit
    /// mode when its row is still a current editable row, otherwise as a
    /// plain draft. A deletion just unlocks.
    fn restore_after_mutation(&mut self, mutation: PendingRemoteMessageMutation) {
        let MutationKind::Edit {
            draft_source,
            original,
            ..
        } = mutation.kind
        else {
            return;
        };
        if self.composer.draft.trim().is_empty() && self.composer.editing.is_none() {
            self.composer.draft = draft_source;
            if self.editable_row(&mutation.message).is_some() {
                self.composer.editing = Some(RemoteEditSelection {
                    target: mutation.message,
                    original_source: original.to_markdown_lite(),
                    original_body: original,
                });
            }
        }
    }
}

impl PendingRemoteMessageMutation {
    const fn label(&self) -> RemoteMutationLabel {
        match self.kind {
            MutationKind::Edit { .. } => RemoteMutationLabel::Edit,
            MutationKind::Delete => RemoteMutationLabel::Delete,
        }
    }
}

impl RemoteChatState {
    pub(in crate::features::chat) fn begin_edit(
        &mut self,
        index: usize,
        message_index: usize,
        capabilities: RemoteCapabilities,
    ) -> Result<(), RemoteSendError> {
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .begin_edit(message_index, capabilities)
    }

    pub(in crate::features::chat) fn queue_edit(
        &mut self,
        index: usize,
        capabilities: RemoteCapabilities,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.conversations.get(index).is_none() {
            return Err(RemoteSendError::Missing);
        }
        let submission = self.allocate_submission_id()?;
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .queue_edit(submission, capabilities)
    }

    pub(in crate::features::chat) fn queue_delete(
        &mut self,
        index: usize,
        message: &ProviderResourceRef,
        capabilities: RemoteCapabilities,
    ) -> Result<RemoteSendRequest, RemoteSendError> {
        if self.conversations.get(index).is_none() {
            return Err(RemoteSendError::Missing);
        }
        let submission = self.allocate_submission_id()?;
        self.conversations
            .get_mut(index)
            .ok_or(RemoteSendError::Missing)?
            .queue_delete(submission, message, capabilities)
    }

    /// The chat and message a delete confirmation was opened for, as long as
    /// that row can still be deleted.
    pub(in crate::features::chat) fn can_still_delete(
        &self,
        index: usize,
        message: &ProviderResourceRef,
        capabilities: RemoteCapabilities,
    ) -> bool {
        self.conversations.get(index).is_some_and(|conversation| {
            capabilities.chat_message_delete.is_offered()
                && !conversation.mutation_gated()
                && conversation.own_row(message).is_some()
        })
    }

    /// The chat target, message target and display text of a row.
    pub(in crate::features::chat) fn delete_candidate(
        &self,
        index: usize,
        message_index: usize,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef, String)> {
        let conversation = self.conversations.get(index)?;
        let target = conversation
            .message_reaction_metadata
            .get(message_index)?
            .target
            .clone()?;
        let preview = conversation.messages.get(message_index)?.text.clone();
        Some((conversation.target.clone(), target, preview))
    }

    pub(in crate::features::chat) fn index_of(&self, chat: &ProviderResourceRef) -> Option<usize> {
        self.conversations
            .iter()
            .position(|conversation| conversation.target == *chat)
    }

    pub(in crate::features::chat) fn message_actions_view(
        &self,
        index: usize,
        message_index: usize,
        capabilities: RemoteCapabilities,
    ) -> Option<RemoteMessageActionsView> {
        self.conversations
            .get(index)?
            .message_actions_view(message_index, capabilities)
    }

    pub(in crate::features::chat) fn has_mutation_submission(
        &self,
        submission: RemoteSubmissionId,
    ) -> bool {
        self.conversations
            .iter()
            .any(|conversation| conversation.has_mutation_submission(submission))
    }

    pub(in crate::features::chat) fn bind_mutation_submission(
        &mut self,
        submission: RemoteSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.conversations
            .iter_mut()
            .any(|conversation| conversation.bind_mutation_submission(submission, intent_id))
    }

    pub(in crate::features::chat) fn has_dispatchable_mutation(
        &self,
        selected_index: Option<usize>,
        submission: RemoteSubmissionId,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
    ) -> bool {
        selected_index
            .and_then(|index| self.conversations.get(index))
            .is_some_and(|conversation| {
                conversation.target == *chat
                    && conversation.has_dispatchable_mutation(submission, message)
            })
    }

    /// Restores a request that was pruned before it reached the controller.
    pub(in crate::features::chat) fn restore_unbound_mutation_for_submission(
        &mut self,
        submission: RemoteSubmissionId,
    ) {
        for conversation in &mut self.conversations {
            if conversation.has_mutation_submission(submission) {
                conversation.restore_unbound_mutation();
            }
        }
    }

    pub(in crate::features::chat) fn mutation_delivered(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_mutation_delivered(intent_id))
    }

    pub(in crate::features::chat) fn mutation_owned(&self, intent_id: WorkspaceIntentId) -> bool {
        self.conversations
            .iter()
            .chain(&self.hidden_conversations)
            .any(|conversation| conversation.has_mutation_intent(intent_id))
    }

    pub(in crate::features::chat) fn mutation_rejected(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationLabel> {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .find_map(|conversation| conversation.resolve_mutation_rejected(intent_id))
    }

    pub(in crate::features::chat) fn mutation_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .any(|conversation| conversation.resolve_mutation_uncertain(intent_id))
    }

    pub(in crate::features::chat) fn complete_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationResolution> {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .find_map(|conversation| conversation.complete_mutation_reconciliation(intent_id))
    }

    pub(in crate::features::chat) fn fail_mutation_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> Option<RemoteMutationLabel> {
        self.conversations
            .iter_mut()
            .chain(&mut self.hidden_conversations)
            .find_map(|conversation| conversation.fail_mutation_reconciliation(intent_id))
    }
}
