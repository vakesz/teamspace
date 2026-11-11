//! Focused state primitives for Microsoft 365 channel delivery.
//!
//! The reply submission identifier is intentionally feature-local.  It is
//! correlated with a transport-owned intent only after the command enters the
//! serial workspace queue, and must never form part of a Microsoft Graph route.

use crate::integrations::{ProviderResourceRef, WorkspaceIntentId};

/// Identifies one locally queued reply-composer submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelReplySubmissionId(u64);

impl RemoteChannelReplySubmissionId {
    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

/// Identifies one locally queued root-post submission. It shares the same
/// focused delivery state as replies, but stays a distinct type so a late
/// result cannot cross mutation shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelPostSubmissionId(u64);

impl RemoteChannelPostSubmissionId {
    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

/// The conservative feature-side text-only cap for both supported channel
/// mutation shapes.
pub(super) const MAX_REMOTE_CHANNEL_MESSAGE_BYTES: usize = 64 * 1024;

/// An exact opaque target for a root post or for a reply to a root post. A
/// nested reply has no representation in this module by design.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RemoteChannelDeliveryTarget {
    RootPost {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
    Reply {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    },
}

impl RemoteChannelDeliveryTarget {
    pub(super) fn root_post(team: ProviderResourceRef, channel: ProviderResourceRef) -> Self {
        Self::RootPost { team, channel }
    }

    pub(super) fn reply(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> Self {
        Self::Reply {
            team,
            channel,
            post,
        }
    }

    fn kind(&self) -> RemoteChannelDeliveryKind {
        match self {
            Self::RootPost { .. } => RemoteChannelDeliveryKind::RootPost,
            Self::Reply { .. } => RemoteChannelDeliveryKind::Reply,
        }
    }
}

/// A provider-neutral root-post request. The visible remote feed remains
/// projection-owned; this request cannot add an optimistic row.
pub(super) struct RemoteChannelPostSendRequest {
    pub(super) submission: RemoteChannelPostSubmissionId,
    pub(super) team: ProviderResourceRef,
    pub(super) channel: ProviderResourceRef,
    pub(super) body: String,
}

/// A provider-neutral reply request for one selected remote root post.
pub(super) struct RemoteChannelReplySendRequest {
    pub(super) submission: RemoteChannelReplySubmissionId,
    pub(super) team: ProviderResourceRef,
    pub(super) channel: ProviderResourceRef,
    pub(super) post: ProviderResourceRef,
    pub(super) body: String,
}

/// Shared validation/lifecycle errors. The caller supplies selection and
/// archived-channel checks; the delivery state owns byte-preserving drafts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteChannelDeliveryError {
    MissingSelection,
    Archived,
    Pending,
    Empty,
    NullByte,
    TooLong,
    IdentifierExhausted,
}

impl RemoteChannelDeliveryError {
    pub(super) const fn user_message(self) -> &'static str {
        match self {
            Self::MissingSelection => "That Microsoft 365 channel target is no longer available.",
            Self::Archived => "Archived Microsoft 365 channels cannot receive new messages.",
            Self::Pending => "Wait for the previous Microsoft 365 channel message to finish.",
            Self::Empty => "A Microsoft 365 channel message cannot be empty.",
            Self::NullByte => "That channel message contains an unsupported null character.",
            Self::TooLong => "That channel message is too long to send to Microsoft 365.",
            Self::IdentifierExhausted => {
                "This session cannot allocate another remote channel message."
            }
        }
    }
}

/// The sole active remote channel mutation, plus inactive drafts retained by
/// opaque target across navigation.
#[derive(Default)]
pub(super) struct RemoteChannelDeliveryState {
    active: Option<RemoteChannelDeliveryComposer>,
    drafts: Vec<RemoteChannelDeliveryComposer>,
    next_submission_id: u64,
}

pub(super) struct RemoteChannelDeliveryComposer {
    target: RemoteChannelDeliveryTarget,
    pub(super) draft: String,
    pub(super) pending: Option<PendingRemoteChannelDelivery>,
    pub(super) delivery_unknown: Option<RemoteChannelDeliveryUnknown>,
}

pub(super) struct PendingRemoteChannelDelivery {
    submission: RemoteChannelDeliverySubmission,
    body: String,
    intent_id: Option<WorkspaceIntentId>,
}

/// A provider timeout/server result that could have followed a successful
/// send. Bounded feeds must never decide whether this message was delivered.
pub(super) struct RemoteChannelDeliveryUnknown {
    body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelDeliverySubmission {
    RootPost(RemoteChannelPostSubmissionId),
    Reply(RemoteChannelReplySubmissionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelDeliveryKind {
    RootPost,
    Reply,
}

impl RemoteChannelDeliveryState {
    /// An explicit disconnect/profile boundary drops every session-only
    /// draft/outcome. A lost-worker clear deliberately does not call this.
    pub(super) fn clear(&mut self) {
        self.active = None;
        self.drafts.clear();
    }

    /// Preserves a non-active current composer under its exact opaque target.
    /// A pending or unknown outcome remains active so it can only resolve the
    /// original request after navigation.
    pub(super) fn stash_idle(&mut self) {
        let Some(composer) = self.active.take() else {
            return;
        };
        if composer.has_active_send() {
            self.active = Some(composer);
            return;
        }
        if composer.draft.is_empty() {
            return;
        }
        if let Some(existing) = self
            .drafts
            .iter()
            .position(|candidate| candidate.target == composer.target)
        {
            self.drafts.swap_remove(existing);
        }
        self.drafts.push(composer);
    }

    /// At most one pending/unknown remote channel mutation exists globally.
    pub(super) fn has_active_elsewhere(&self, target: &RemoteChannelDeliveryTarget) -> bool {
        self.active
            .as_ref()
            .is_some_and(|composer| composer.has_active_send() && composer.target != *target)
    }

    /// Restores or creates a target-bound idle composer. It never replaces a
    /// pending/unknown composer for some other root post or channel.
    pub(super) fn composer_for_target_mut(
        &mut self,
        target: &RemoteChannelDeliveryTarget,
    ) -> Option<&mut RemoteChannelDeliveryComposer> {
        if self
            .active
            .as_ref()
            .is_some_and(|composer| composer.target == *target)
        {
            return self.active.as_mut();
        }
        if self
            .active
            .as_ref()
            .is_some_and(RemoteChannelDeliveryComposer::has_active_send)
        {
            return None;
        }
        self.stash_idle();
        self.active = self.take_idle(target).or_else(|| {
            Some(RemoteChannelDeliveryComposer {
                target: target.clone(),
                draft: String::new(),
                pending: None,
                delivery_unknown: None,
            })
        });
        self.active.as_mut()
    }

    #[cfg(test)]
    pub(super) fn composer_for_target(
        &self,
        target: &RemoteChannelDeliveryTarget,
    ) -> Option<&RemoteChannelDeliveryComposer> {
        self.active
            .as_ref()
            .filter(|composer| composer.target == *target)
            .or_else(|| {
                self.drafts
                    .iter()
                    .find(|composer| composer.target == *target)
            })
    }

    pub(super) fn queue_root_post(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> Result<RemoteChannelPostSendRequest, RemoteChannelDeliveryError> {
        let target = RemoteChannelDeliveryTarget::root_post(team.clone(), channel.clone());
        let body = self.queue_body(&target)?;
        let submission = RemoteChannelPostSubmissionId(self.allocate_submission_id()?);
        self.mark_pending(
            &target,
            RemoteChannelDeliverySubmission::RootPost(submission),
            body.clone(),
        );
        Ok(RemoteChannelPostSendRequest {
            submission,
            team,
            channel,
            body,
        })
    }

    pub(super) fn queue_reply(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> Result<RemoteChannelReplySendRequest, RemoteChannelDeliveryError> {
        let target =
            RemoteChannelDeliveryTarget::reply(team.clone(), channel.clone(), post.clone());
        let body = self.queue_body(&target)?;
        let submission = RemoteChannelReplySubmissionId(self.allocate_submission_id()?);
        self.mark_pending(
            &target,
            RemoteChannelDeliverySubmission::Reply(submission),
            body.clone(),
        );
        Ok(RemoteChannelReplySendRequest {
            submission,
            team,
            channel,
            post,
            body,
        })
    }

    pub(super) fn bind_root_post_submission(
        &mut self,
        submission: RemoteChannelPostSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.bind_submission(
            RemoteChannelDeliverySubmission::RootPost(submission),
            intent_id,
        )
    }

    pub(super) fn bind_reply_submission(
        &mut self,
        submission: RemoteChannelReplySubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.bind_submission(
            RemoteChannelDeliverySubmission::Reply(submission),
            intent_id,
        )
    }

    pub(super) fn root_post_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_delivered(RemoteChannelDeliveryKind::RootPost, intent_id)
    }

    pub(super) fn reply_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_delivered(RemoteChannelDeliveryKind::Reply, intent_id)
    }

    pub(super) fn root_post_retry_scheduled(&self, intent_id: WorkspaceIntentId) -> bool {
        self.has_pending_intent(RemoteChannelDeliveryKind::RootPost, intent_id)
    }

    pub(super) fn reply_retry_scheduled(&self, intent_id: WorkspaceIntentId) -> bool {
        self.has_pending_intent(RemoteChannelDeliveryKind::Reply, intent_id)
    }

    pub(super) fn root_post_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_terminal_failure(RemoteChannelDeliveryKind::RootPost, intent_id)
    }

    pub(super) fn reply_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_terminal_failure(RemoteChannelDeliveryKind::Reply, intent_id)
    }

    pub(super) fn root_post_delivery_uncertain(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_delivery_unknown(RemoteChannelDeliveryKind::RootPost, intent_id)
    }

    pub(super) fn reply_delivery_uncertain(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.resolve_delivery_unknown(RemoteChannelDeliveryKind::Reply, intent_id)
    }

    pub(super) fn make_unknown_editable(&mut self, target: &RemoteChannelDeliveryTarget) -> bool {
        let Some(composer) = self.active.as_mut() else {
            return false;
        };
        composer.target == *target && composer.make_delivery_unknown_editable()
    }

    #[cfg(test)]
    pub(super) fn has_active_root_post_for_test(&self) -> bool {
        self.has_active_kind(RemoteChannelDeliveryKind::RootPost)
    }

    #[cfg(test)]
    pub(super) fn has_active_reply_for_test(&self) -> bool {
        self.has_active_kind(RemoteChannelDeliveryKind::Reply)
    }

    #[cfg(test)]
    pub(super) fn has_unknown_root_post_for_test(&self) -> bool {
        self.has_unknown_kind(RemoteChannelDeliveryKind::RootPost)
    }

    #[cfg(test)]
    pub(super) fn has_unknown_reply_for_test(&self) -> bool {
        self.has_unknown_kind(RemoteChannelDeliveryKind::Reply)
    }

    fn queue_body(
        &mut self,
        target: &RemoteChannelDeliveryTarget,
    ) -> Result<String, RemoteChannelDeliveryError> {
        if self.has_active_elsewhere(target) {
            return Err(RemoteChannelDeliveryError::Pending);
        }
        let composer = self
            .composer_for_target_mut(target)
            .expect("a checked inactive remote target must retain its composer");
        if composer.has_active_send() {
            return Err(RemoteChannelDeliveryError::Pending);
        }
        if composer.draft.trim().is_empty() {
            return Err(RemoteChannelDeliveryError::Empty);
        }
        // Trimming is validation-only. A provider rejection or explicit
        // unknown-to-editable review restores these exact user bytes.
        let body = composer.draft.clone();
        if body.contains('\0') {
            return Err(RemoteChannelDeliveryError::NullByte);
        }
        if body.len() > MAX_REMOTE_CHANNEL_MESSAGE_BYTES {
            return Err(RemoteChannelDeliveryError::TooLong);
        }
        Ok(body)
    }

    fn mark_pending(
        &mut self,
        target: &RemoteChannelDeliveryTarget,
        submission: RemoteChannelDeliverySubmission,
        body: String,
    ) {
        let composer = self
            .composer_for_target_mut(target)
            .expect("a checked remote target must retain its composer while queuing");
        composer.draft.clear();
        composer.pending = Some(PendingRemoteChannelDelivery {
            submission,
            body,
            intent_id: None,
        });
    }

    fn take_idle(
        &mut self,
        target: &RemoteChannelDeliveryTarget,
    ) -> Option<RemoteChannelDeliveryComposer> {
        self.drafts
            .iter()
            .position(|candidate| candidate.target == *target)
            .map(|index| self.drafts.swap_remove(index))
    }

    fn allocate_submission_id(&mut self) -> Result<u64, RemoteChannelDeliveryError> {
        let next = self
            .next_submission_id
            .checked_add(1)
            .ok_or(RemoteChannelDeliveryError::IdentifierExhausted)?;
        self.next_submission_id = next;
        Ok(next)
    }

    fn bind_submission(
        &mut self,
        submission: RemoteChannelDeliverySubmission,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        let Some(composer) = self.active.as_mut() else {
            return false;
        };
        let Some(pending) = composer.pending.as_mut() else {
            return false;
        };
        if pending.submission != submission || pending.intent_id.is_some() {
            return false;
        }
        pending.intent_id = Some(intent_id);
        true
    }

    fn has_pending_intent(
        &self,
        kind: RemoteChannelDeliveryKind,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.active.as_ref().is_some_and(|composer| {
            composer.target.kind() == kind && composer.has_pending_intent(intent_id)
        })
    }

    fn resolve_delivered(
        &mut self,
        kind: RemoteChannelDeliveryKind,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        let Some(composer) = self.active.as_mut() else {
            return false;
        };
        if composer.target.kind() != kind || !composer.has_pending_intent(intent_id) {
            return false;
        }
        // `201 Created` affects only the composer. A later normalized Graph
        // projection, never an optimistic local item, controls the feed.
        composer.pending = None;
        true
    }

    fn resolve_terminal_failure(
        &mut self,
        kind: RemoteChannelDeliveryKind,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        let Some(composer) = self.active.as_mut() else {
            return false;
        };
        if composer.target.kind() != kind {
            return false;
        }
        let Some(pending) = composer.pending.take() else {
            return false;
        };
        if pending.intent_id != Some(intent_id) {
            composer.pending = Some(pending);
            return false;
        }
        composer.draft = pending.body;
        true
    }

    fn resolve_delivery_unknown(
        &mut self,
        kind: RemoteChannelDeliveryKind,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        let Some(composer) = self.active.as_mut() else {
            return false;
        };
        if composer.target.kind() != kind {
            return false;
        }
        let Some(pending) = composer.pending.take() else {
            return false;
        };
        if pending.intent_id != Some(intent_id) {
            composer.pending = Some(pending);
            return false;
        }
        composer.delivery_unknown = Some(RemoteChannelDeliveryUnknown { body: pending.body });
        true
    }

    #[cfg(test)]
    fn has_active_kind(&self, kind: RemoteChannelDeliveryKind) -> bool {
        self.active
            .as_ref()
            .is_some_and(|composer| composer.target.kind() == kind && composer.has_active_send())
    }

    #[cfg(test)]
    fn has_unknown_kind(&self, kind: RemoteChannelDeliveryKind) -> bool {
        self.active.as_ref().is_some_and(|composer| {
            composer.target.kind() == kind && composer.delivery_unknown.is_some()
        })
    }
}

impl RemoteChannelDeliveryComposer {
    fn has_active_send(&self) -> bool {
        self.pending.is_some() || self.delivery_unknown.is_some()
    }

    fn has_pending_intent(&self, intent_id: WorkspaceIntentId) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.intent_id == Some(intent_id))
    }

    fn make_delivery_unknown_editable(&mut self) -> bool {
        let Some(unknown) = self.delivery_unknown.take() else {
            return false;
        };
        self.draft = unknown.body;
        true
    }
}
