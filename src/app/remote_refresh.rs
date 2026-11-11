//! Event-driven refresh policy for Microsoft 365 reads.
//!
//! The Teams API polling rules forbid loop-polling Teams resources, so the app
//! never refreshes on a timer. A refresh follows only an explicit user event:
//! returning to the window, opening a selection, or pressing F5 / Ctrl+R /
//! the header Refresh button. This module is pure policy: it decides which
//! bounded reads an event may start and holds the per-kind minimum interval,
//! while the controller still owns queueing, cooldown and cancellation.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Minimum spacing between two directory (Chats) refreshes.
pub(super) const DIRECTORY_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Minimum spacing between two refreshes of one selected conversation.
pub(super) const SELECTED_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Why a refresh may run. Ordered by strength: a stronger event in the same
/// frame subsumes a weaker one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RefreshTrigger {
    /// A remote selection was opened (its own read is already staged).
    SelectionOpened,
    /// The window regained focus.
    FocusGained,
    /// F5, Ctrl+R or the header Refresh button.
    Explicit,
}

/// A kind of read, never a specific chat or channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RefreshTarget {
    Directory,
    SelectedChat,
    SelectedChannel,
    SelectedReplies,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct RefreshPlan {
    targets: Vec<RefreshTarget>,
}

impl RefreshPlan {
    pub(super) fn targets(&self) -> &[RefreshTarget] {
        &self.targets
    }

    pub(super) fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    #[cfg(test)]
    fn contains(&self, target: RefreshTarget) -> bool {
        self.targets.contains(&target)
    }
}

/// The single window-focus signal. egui reports `None` before the first focus
/// event, which counts as focused. Presence, read acknowledgement and refresh
/// all use this one definition.
pub(super) fn window_focused(ctx: &eframe::egui::Context) -> bool {
    ctx.input(|input| input.viewport().focused) != Some(false)
}

pub(super) struct RemoteRefreshScheduler {
    focused: bool,
    pending: Option<RefreshTrigger>,
    last: BTreeMap<RefreshTarget, Instant>,
}

impl Default for RemoteRefreshScheduler {
    fn default() -> Self {
        Self {
            focused: true,
            pending: None,
            last: BTreeMap::new(),
        }
    }
}

impl RemoteRefreshScheduler {
    pub(super) const fn focused(&self) -> bool {
        self.focused
    }

    /// Records this frame's focus and returns true exactly once per
    /// unfocused-to-focused edge, which also signals `FocusGained`.
    pub(super) fn observe_focus(&mut self, focused: bool) -> bool {
        let gained = focused && !self.focused;
        self.focused = focused;
        if gained {
            self.signal(RefreshTrigger::FocusGained);
        }
        gained
    }

    /// Notes an event; only the strongest one per frame is kept.
    pub(super) fn signal(&mut self, trigger: RefreshTrigger) {
        self.pending = Some(self.pending.map_or(trigger, |current| current.max(trigger)));
    }

    /// Records that a read of `target` just started or completed through
    /// another path (a selection read, the bootstrap), so a nearby event does
    /// not read it twice.
    pub(super) fn note_read(&mut self, target: RefreshTarget, now: Instant) {
        self.last.insert(target, now);
    }

    /// Forgets all history, for a new session or account.
    pub(super) fn reset(&mut self) {
        self.pending = None;
        self.last.clear();
    }

    /// Consumes the pending event and returns the reads it allows.
    /// `selected` lists the selected-conversation kinds that exist on the
    /// visible surface, so an absent kind never consumes its interval.
    /// Nothing runs while unfocused. `Explicit` bypasses the minimum
    /// intervals (the controller's Retry-After cooldown still applies).
    pub(super) fn take_plan(&mut self, now: Instant, selected: &[RefreshTarget]) -> RefreshPlan {
        let Some(trigger) = self.pending.take() else {
            return RefreshPlan::default();
        };
        if !self.focused {
            return RefreshPlan::default();
        }
        let mut candidates = vec![RefreshTarget::Directory];
        if trigger != RefreshTrigger::SelectionOpened {
            candidates.extend(
                selected
                    .iter()
                    .copied()
                    .filter(|target| *target != RefreshTarget::Directory),
            );
        }
        let mut plan = RefreshPlan::default();
        for target in candidates {
            let due = trigger == RefreshTrigger::Explicit
                || self.last.get(&target).is_none_or(|last| {
                    now.saturating_duration_since(*last) >= min_interval(target)
                });
            if due {
                self.last.insert(target, now);
                plan.targets.push(target);
            }
        }
        plan
    }
}

const fn min_interval(target: RefreshTarget) -> Duration {
    match target {
        RefreshTarget::Directory => DIRECTORY_MIN_INTERVAL,
        _ => SELECTED_MIN_INTERVAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &[RefreshTarget] = &[RefreshTarget::SelectedChat];

    fn secs(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    #[test]
    fn focus_gain_refreshes_the_directory_and_the_selected_read() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.observe_focus(false);
        assert!(scheduler.observe_focus(true));
        let plan = scheduler.take_plan(now, CHAT);
        assert!(plan.contains(RefreshTarget::Directory));
        assert!(plan.contains(RefreshTarget::SelectedChat));
    }

    #[test]
    fn a_focus_edge_fires_once_not_every_focused_frame() {
        let mut scheduler = RemoteRefreshScheduler::default();
        assert!(!scheduler.observe_focus(true), "startup focus is no edge");
        assert!(!scheduler.observe_focus(false));
        assert!(scheduler.observe_focus(true));
        assert!(!scheduler.observe_focus(true));
        assert!(!scheduler.observe_focus(true));
    }

    #[test]
    fn nothing_runs_while_unfocused_and_the_event_is_dropped() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.observe_focus(false);
        scheduler.signal(RefreshTrigger::Explicit);
        assert!(scheduler.take_plan(now, CHAT).is_empty());
        scheduler.observe_focus(true);
        scheduler.take_plan(now, CHAT);
        assert!(scheduler.take_plan(secs(now, 1), CHAT).is_empty());
    }

    #[test]
    fn minimum_intervals_hold_for_automatic_triggers() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.signal(RefreshTrigger::FocusGained);
        assert_eq!(scheduler.take_plan(now, CHAT).targets().len(), 2);

        scheduler.signal(RefreshTrigger::FocusGained);
        assert!(scheduler.take_plan(secs(now, 29), CHAT).is_empty());

        scheduler.signal(RefreshTrigger::FocusGained);
        let plan = scheduler.take_plan(secs(now, 30), CHAT);
        assert_eq!(plan.targets(), [RefreshTarget::SelectedChat]);

        scheduler.signal(RefreshTrigger::FocusGained);
        let plan = scheduler.take_plan(secs(now, 60), CHAT);
        assert!(plan.contains(RefreshTarget::Directory), "60 s elapsed");
    }

    #[test]
    fn explicit_bypasses_the_minimum_intervals() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.signal(RefreshTrigger::FocusGained);
        scheduler.take_plan(now, CHAT);
        scheduler.signal(RefreshTrigger::Explicit);
        let plan = scheduler.take_plan(secs(now, 1), CHAT);
        assert!(plan.contains(RefreshTarget::Directory));
        assert!(plan.contains(RefreshTarget::SelectedChat));
    }

    #[test]
    fn opening_a_selection_refreshes_only_the_directory() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.signal(RefreshTrigger::SelectionOpened);
        let plan = scheduler.take_plan(now, CHAT);
        assert_eq!(plan.targets(), [RefreshTarget::Directory]);
    }

    #[test]
    fn a_read_noted_elsewhere_suppresses_a_duplicate() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.note_read(RefreshTarget::Directory, now);
        scheduler.note_read(RefreshTarget::SelectedChat, now);
        scheduler.signal(RefreshTrigger::FocusGained);
        assert!(scheduler.take_plan(secs(now, 5), CHAT).is_empty());
    }

    #[test]
    fn an_absent_selected_kind_does_not_consume_its_interval() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.signal(RefreshTrigger::FocusGained);
        assert_eq!(scheduler.take_plan(now, &[]).targets().len(), 1);
        scheduler.signal(RefreshTrigger::FocusGained);
        let plan = scheduler.take_plan(secs(now, 1), CHAT);
        assert_eq!(plan.targets(), [RefreshTarget::SelectedChat]);
    }

    #[test]
    fn the_strongest_event_in_a_frame_wins_and_reset_forgets_history() {
        let (mut scheduler, now) = (RemoteRefreshScheduler::default(), Instant::now());
        scheduler.signal(RefreshTrigger::FocusGained);
        scheduler.take_plan(now, CHAT);
        scheduler.signal(RefreshTrigger::SelectionOpened);
        scheduler.signal(RefreshTrigger::Explicit);
        scheduler.signal(RefreshTrigger::SelectionOpened);
        assert_eq!(scheduler.take_plan(secs(now, 1), CHAT).targets().len(), 2);
        scheduler.reset();
        scheduler.signal(RefreshTrigger::FocusGained);
        assert_eq!(scheduler.take_plan(secs(now, 2), CHAT).targets().len(), 2);
    }
}
