//! Bounded, provider-neutral remote message-reaction projections.
//!
//! Provider adapters normalize their reaction wire values before constructing
//! this type. Features receive only a closed application palette and cannot
//! turn an arbitrary provider-owned string into a mutation request.

use crate::shared::reaction::ReactionKind;

/// One supported reaction summary from a remote message projection.
///
/// A summary is intentionally aggregate-only: user identities, provider
/// timestamps, custom-reaction metadata, and raw reaction values remain at
/// the adapter boundary. `count` includes the current user's reaction when
/// `reacted_by_current_user` is true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteMessageReaction {
    kind: ReactionKind,
    count: usize,
    reacted_by_current_user: bool,
}

impl RemoteMessageReaction {
    /// Creates one normalized non-empty summary.
    ///
    /// Callers must omit zero-count entries so a missing kind always means
    /// no known provider reaction of that kind in the bounded projection.
    pub(crate) const fn new(
        kind: ReactionKind,
        count: usize,
        reacted_by_current_user: bool,
    ) -> Self {
        Self {
            kind,
            count,
            reacted_by_current_user,
        }
    }

    pub(crate) const fn kind(&self) -> ReactionKind {
        self.kind
    }

    pub(crate) const fn count(&self) -> usize {
        self.count
    }

    pub(crate) const fn reacted_by_current_user(&self) -> bool {
        self.reacted_by_current_user
    }

    /// Test-only spelling that makes provider-projection fixtures read like
    /// a construction boundary rather than a production mutation API.
    #[cfg(test)]
    pub(crate) const fn for_test(
        kind: ReactionKind,
        count: usize,
        reacted_by_current_user: bool,
    ) -> Self {
        Self::new(kind, count, reacted_by_current_user)
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteMessageReaction;
    use crate::shared::reaction::ReactionKind;

    #[test]
    fn summary_retains_only_typed_aggregate_state() {
        let reaction = RemoteMessageReaction::new(ReactionKind::Heart, 3, true);
        assert_eq!(reaction.kind(), ReactionKind::Heart);
        assert_eq!(reaction.count(), 3);
        assert!(reaction.reacted_by_current_user());
    }
}
