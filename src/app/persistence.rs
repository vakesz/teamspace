//! App-owned lifecycle mechanics for local feature snapshots.
//!
//! Feature modules own their schemas and concrete storage adapters. This
//! module owns the shared authenticated-profile lifecycle: one-time setup,
//! debounced writes, retry scheduling, and cleanup. It keeps that behavior
//! behind one small interface so each feature adapter does not reimplement it.

use eframe::egui;
use std::fmt::Display;
use std::time::{Duration, Instant};

const SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const SAVE_RETRY_DELAY: Duration = Duration::from_secs(5);

/// The app-owned lifecycle state around one feature-owned local adapter.
///
/// A caller first calls [`Self::begin_initialization`], then attaches an
/// adapter as writable or protected. The adapter remains feature-specific;
/// only the common lifecycle mechanics live here.
pub(crate) struct LocalFeatureStore<S> {
    initialized: bool,
    store: Option<S>,
    writable: bool,
    next_save_at: Option<Instant>,
    save_error_shown: bool,
    /// The most recent save failure, kept until a save succeeds so the app can
    /// show a persistent (non-toast) warning, for example when a snapshot
    /// outgrows its size limit.
    failure: Option<String>,
    /// Skip the debounce and retry delay on the next save opportunity.
    force_next_save: bool,
}

impl<S> Default for LocalFeatureStore<S> {
    fn default() -> Self {
        Self {
            initialized: false,
            store: None,
            writable: false,
            next_save_at: None,
            save_error_shown: false,
            failure: None,
            force_next_save: false,
        }
    }
}

/// The result of one save opportunity. `Failed` reports whether the app
/// should present a new user-facing notification for this retry sequence.
pub(crate) enum SaveAttempt<E> {
    Idle,
    Deferred,
    Saved,
    Unavailable,
    Failed { error: E, notify_user: bool },
}

impl<S> LocalFeatureStore<S> {
    /// Marks this lifecycle initialized once and reports whether the caller
    /// should now construct and load its feature-owned adapter.
    pub(crate) fn begin_initialization(&mut self) -> bool {
        if self.initialized {
            return false;
        }
        self.initialized = true;
        true
    }

    /// Attaches an adapter that recovered successfully or has no saved data.
    pub(crate) fn allow_writes(&mut self, store: S) {
        self.store = Some(store);
        self.writable = true;
    }

    /// Keeps an adapter available for cleanup while deliberately protecting a
    /// snapshot that could not be recovered from accidental replacement.
    pub(crate) fn protect_existing_data(&mut self, store: S) {
        self.store = Some(store);
        self.writable = false;
    }

    /// Saves a dirty feature projection after a short debounce. Snapshot
    /// construction and feature-specific write logic stay with the caller;
    /// scheduling and error suppression stay local to this module.
    pub(crate) fn save_if_due<Snapshot, E: Display>(
        &mut self,
        ctx: &egui::Context,
        dirty: bool,
        snapshot: impl FnOnce() -> Snapshot,
        save: impl FnOnce(&S, &Snapshot) -> Result<(), E>,
    ) -> SaveAttempt<E> {
        if !dirty {
            self.next_save_at = None;
            self.save_error_shown = false;
            self.failure = None;
            self.force_next_save = false;
            return SaveAttempt::Idle;
        }
        if !self.writable {
            return SaveAttempt::Unavailable;
        }
        let Some(store) = self.store.as_ref() else {
            return SaveAttempt::Unavailable;
        };

        let now = Instant::now();
        let save_at = *self.next_save_at.get_or_insert(now + SAVE_DEBOUNCE);
        let forced = std::mem::take(&mut self.force_next_save);
        if now < save_at && !forced {
            ctx.request_repaint_after(save_at.saturating_duration_since(now));
            return SaveAttempt::Deferred;
        }

        let snapshot = snapshot();
        match save(store, &snapshot) {
            Ok(()) => {
                self.next_save_at = None;
                self.save_error_shown = false;
                self.failure = None;
                SaveAttempt::Saved
            }
            Err(error) => {
                self.failure = Some(error.to_string());
                let notify_user = !self.save_error_shown;
                self.save_error_shown = true;
                self.next_save_at = Some(now + SAVE_RETRY_DELAY);
                ctx.request_repaint_after(SAVE_RETRY_DELAY);
                SaveAttempt::Failed { error, notify_user }
            }
        }
    }

    /// Detaches the current adapter and resets the lifecycle for a future
    /// profile. The caller owns the feature-specific clear operation.
    pub(crate) fn take_for_cleanup(&mut self) -> Option<S> {
        self.initialized = false;
        self.writable = false;
        self.next_save_at = None;
        self.save_error_shown = false;
        self.failure = None;
        self.force_next_save = false;
        self.store.take()
    }

    /// Makes the next [`Self::save_if_due`] write immediately, bypassing the
    /// debounce and any retry delay. Used when the app is about to exit.
    pub(crate) fn force_next_save(&mut self) {
        self.force_next_save = true;
    }

    /// The last save failure, if the most recent attempt did not succeed.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Reports whether this feature recovered a writable local adapter.
    /// App-owned migration orchestration uses this to avoid acknowledging a
    /// legacy snapshot whose source could not be read safely.
    pub(crate) const fn is_writable(&self) -> bool {
        self.writable
    }

    #[cfg(test)]
    pub(crate) fn with_writable(store: S) -> Self {
        Self {
            initialized: true,
            store: Some(store),
            writable: true,
            next_save_at: None,
            save_error_shown: false,
            failure: None,
            force_next_save: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn make_save_due(&mut self) {
        self.next_save_at = Some(Instant::now());
    }

    #[cfg(test)]
    pub(crate) const fn is_initialized(&self) -> bool {
        self.initialized
    }

    #[cfg(test)]
    pub(crate) const fn has_store(&self) -> bool {
        self.store.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalFeatureStore, SaveAttempt};
    use eframe::egui;

    #[test]
    fn lifecycle_initializes_once_and_cleanup_resets_it() {
        let mut lifecycle = LocalFeatureStore::<u8>::default();

        assert!(lifecycle.begin_initialization());
        assert!(!lifecycle.begin_initialization());
        lifecycle.allow_writes(4);
        assert_eq!(lifecycle.take_for_cleanup(), Some(4));
        assert!(!lifecycle.is_initialized());
        assert!(!lifecycle.has_store());
        assert!(!lifecycle.is_writable());
    }

    #[test]
    fn dirty_saves_are_debounced_and_then_marked_saved() {
        let mut lifecycle = LocalFeatureStore::with_writable(3_u8);
        let context = egui::Context::default();
        let mut writes = Vec::new();

        assert!(matches!(
            lifecycle.save_if_due(
                &context,
                true,
                || "snapshot",
                |store, snapshot| {
                    writes.push((*store, *snapshot));
                    Ok::<(), &str>(())
                }
            ),
            SaveAttempt::Deferred
        ));
        assert!(writes.is_empty());

        lifecycle.make_save_due();
        assert!(matches!(
            lifecycle.save_if_due(
                &context,
                true,
                || "snapshot",
                |store, snapshot| {
                    writes.push((*store, *snapshot));
                    Ok::<(), &str>(())
                }
            ),
            SaveAttempt::Saved
        ));
        assert_eq!(writes, [(3, "snapshot")]);
    }

    #[test]
    fn failed_saves_notify_once_per_retry_sequence() {
        let mut lifecycle = LocalFeatureStore::with_writable(());
        let context = egui::Context::default();
        lifecycle.make_save_due();

        let first = lifecycle.save_if_due(&context, true, || (), |_, _| Err("offline"));
        assert!(matches!(
            first,
            SaveAttempt::Failed {
                error: "offline",
                notify_user: true
            }
        ));

        lifecycle.make_save_due();
        let second = lifecycle.save_if_due(&context, true, || (), |_, _| Err("offline"));
        assert!(matches!(
            second,
            SaveAttempt::Failed {
                error: "offline",
                notify_user: false
            }
        ));

        assert!(matches!(
            lifecycle.save_if_due(&context, false, || (), |_, _| Ok::<(), &str>(())),
            SaveAttempt::Idle
        ));
    }

    #[test]
    fn forcing_a_save_bypasses_the_debounce_and_retry_delay() {
        let mut lifecycle = LocalFeatureStore::with_writable(());
        let context = egui::Context::default();

        assert!(matches!(
            lifecycle.save_if_due(&context, true, || (), |_, _| Ok::<(), &str>(())),
            SaveAttempt::Deferred
        ));
        lifecycle.force_next_save();
        assert!(matches!(
            lifecycle.save_if_due(&context, true, || (), |_, _| Ok::<(), &str>(())),
            SaveAttempt::Saved
        ));

        // A failed save schedules a retry delay; forcing skips it too.
        lifecycle.force_next_save();
        assert!(matches!(
            lifecycle.save_if_due(&context, true, || (), |_, _| Err("full")),
            SaveAttempt::Failed { .. }
        ));
        assert!(matches!(
            lifecycle.save_if_due(&context, true, || (), |_, _| Ok::<(), &str>(())),
            SaveAttempt::Deferred
        ));
        lifecycle.force_next_save();
        assert!(matches!(
            lifecycle.save_if_due(&context, true, || (), |_, _| Ok::<(), &str>(())),
            SaveAttempt::Saved
        ));
    }

    #[test]
    fn a_save_failure_is_remembered_until_a_save_succeeds() {
        let mut lifecycle = LocalFeatureStore::with_writable(());
        let context = egui::Context::default();
        assert_eq!(lifecycle.failure(), None);

        lifecycle.force_next_save();
        let _ = lifecycle.save_if_due(&context, true, || (), |_, _| Err("file is too large"));
        assert_eq!(lifecycle.failure(), Some("file is too large"));

        // Still reported while retries are deferred.
        let _ = lifecycle.save_if_due(&context, true, || (), |_, _| Err("file is too large"));
        assert_eq!(lifecycle.failure(), Some("file is too large"));

        lifecycle.force_next_save();
        let _ = lifecycle.save_if_due(&context, true, || (), |_, _| Ok::<(), &str>(()));
        assert_eq!(lifecycle.failure(), None);
    }
}
