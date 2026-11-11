//! Session-only detection of new Microsoft 365 messages from directory
//! previews, for in-app badges and toasts.
//!
//! Nothing here is persisted or sent anywhere. The first projection after a
//! connect seeds silently. After that a chat is "new" only when its newest
//! preview is a different message AND strictly later than the one already
//! seen, and it is not the user's own, not deleted, and not the open chat
//! while the window is focused. A mention is claimed only from structured
//! history, never from preview text.

use super::{RemoteChatState, RemoteConversation};
use crate::integrations::{ProviderResourceRef, RemoteMessageInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::features::chat) enum NewActivityKind {
    Message,
    Mention,
}

/// One chat's arrivals since the last drain. `chat_index_hint` is only a
/// position into the current directory; match by target before relying on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::features::chat) struct NewActivity {
    pub(in crate::features::chat) chat_index_hint: usize,
    pub(in crate::features::chat) kind: NewActivityKind,
    pub(in crate::features::chat) count: usize,
}

#[derive(Debug)]
pub(super) struct PendingArrival {
    target: ProviderResourceRef,
    count: usize,
}

impl RemoteConversation {
    /// Advances this chat's watermark to its newest preview and reports
    /// whether that preview is a genuine new arrival worth announcing. The
    /// watermark moves forward even when the arrival is suppressed, so the
    /// same message can never announce itself twice.
    pub(super) fn observe_preview_arrival(
        &mut self,
        seeding: bool,
        floor: Option<RemoteMessageInstant>,
    ) -> bool {
        let (Some(target), Some(instant)) = (self.preview_target.clone(), self.preview_instant)
        else {
            // Without both a target and a time nothing can be proven newer.
            return false;
        };
        let newer = !seeding
            && match &self.notify_watermark {
                Some((seen, at)) => *seen != target && instant > *at,
                // A chat first seen after seeding counts only if it is newer
                // than everything the session started with.
                None => floor.is_some_and(|floor| instant > floor),
            };
        if self
            .notify_watermark
            .as_ref()
            .is_none_or(|(_, at)| instant > *at)
        {
            self.notify_watermark = Some((target, instant));
        }
        newer && !self.preview_suppressed
    }

    /// Whether structured history proves that its newest message, the one
    /// just announced, mentions the signed-in user.
    fn newest_announced_mentions_me(&self) -> bool {
        self.message_reaction_metadata
            .last()
            .is_some_and(|newest| newest.mentioned_current_user && !newest.deleted)
            && self.history_newest_message.is_some()
            && self.history_newest_message.as_ref()
                == self.notify_watermark.as_ref().map(|(target, _)| target)
    }
}

impl RemoteChatState {
    /// Runs after a directory replacement. `selected` is the open chat's
    /// target, and `viewing` is whether the window is focused.
    pub(super) fn observe_arrivals(
        &mut self,
        selected: Option<&ProviderResourceRef>,
        viewing: bool,
    ) {
        let seeding = !self.seeded;
        if seeding {
            self.arrival_floor = self
                .conversations
                .iter()
                .filter_map(|conversation| conversation.preview_instant)
                .max();
        }
        let floor = self.arrival_floor;
        let mut announced = Vec::new();
        for conversation in &mut self.conversations {
            if !conversation.observe_preview_arrival(seeding, floor) {
                continue;
            }
            if selected == Some(&conversation.target) && viewing {
                continue;
            }
            conversation.new_count = conversation.new_count.saturating_add(1);
            announced.push(conversation.target.clone());
        }
        for target in announced {
            match self
                .pending_arrivals
                .iter_mut()
                .find(|pending| pending.target == target)
            {
                Some(pending) => pending.count = pending.count.saturating_add(1),
                None => self
                    .pending_arrivals
                    .push(PendingArrival { target, count: 1 }),
            }
        }
        self.seeded = true;
    }

    /// Forgets session arrival state at a connect, account or transport
    /// boundary, so the next projection seeds silently again.
    pub(super) fn reset_arrivals(&mut self) {
        self.seeded = false;
        self.arrival_floor = None;
        self.pending_arrivals.clear();
    }

    pub(in crate::features::chat) fn take_new_activity(&mut self) -> Vec<NewActivity> {
        let pending = std::mem::take(&mut self.pending_arrivals);
        pending
            .into_iter()
            .filter_map(|arrival| {
                let index = self
                    .conversations
                    .iter()
                    .position(|conversation| conversation.target == arrival.target)?;
                let kind = if self.conversations[index].newest_announced_mentions_me() {
                    NewActivityKind::Mention
                } else {
                    NewActivityKind::Message
                };
                Some(NewActivity {
                    chat_index_hint: index,
                    kind,
                    count: arrival.count,
                })
            })
            .collect()
    }
}
