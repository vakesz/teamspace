//! Lifecycle of the app's on-device feature stores: lazy initialization after
//! sign-in, throttled saves, and cleanup on sign-out. The shared mechanics
//! live in `persistence::LocalFeatureStore`; this module only wires each
//! feature-owned adapter to it.

use std::fmt::Display;

use super::TeamsApp;
use crate::app::app_prefs::LocalAppPrefsStore;
use crate::app::auth::AuthState;
use crate::app::persistence::{LocalFeatureStore, SaveAttempt};
use crate::app::presence::LocalPresenceStore;
use crate::features::activity::LocalActivityStore;
use crate::features::calendar::LocalCalendarStore;
use crate::features::calls::LocalCallsStore;
use crate::features::chat::{ChatState, LocalChatPrefsStore, LocalChatStore};
use crate::features::files::LocalFilesStore;
use crate::features::teams::LocalTeamsStore;
use crate::features::threads::LocalThreadStore;
use crate::ui::toast::ToastState;
use eframe::egui;

/// One lifecycle per local feature store.
#[derive(Default)]
pub(super) struct LocalStores {
    pub(super) presence_store: LocalFeatureStore<LocalPresenceStore>,
    pub(super) chat_store: LocalFeatureStore<LocalChatStore>,
    pub(super) chat_prefs_store: LocalFeatureStore<LocalChatPrefsStore>,
    pub(super) app_prefs_store: LocalFeatureStore<LocalAppPrefsStore>,
    pub(super) calendar_store: LocalFeatureStore<LocalCalendarStore>,
    pub(super) activity_store: LocalFeatureStore<LocalActivityStore>,
    pub(super) teams_store: LocalFeatureStore<LocalTeamsStore>,
    pub(super) threads_store: LocalFeatureStore<LocalThreadStore>,
    pub(super) calls_store: LocalFeatureStore<LocalCallsStore>,
    pub(super) files_store: LocalFeatureStore<LocalFilesStore>,
}

/// Session flags for importing the old Activity/Teams discussion overlays
/// into the canonical thread store.
#[derive(Default)]
pub(super) struct ThreadMigration {
    pub(super) error_shown: bool,
    /// True only when the loaded or just-saved canonical snapshot already
    /// contains both source-migration markers. It must never be inferred from
    /// an in-memory import that has not reached disk yet.
    pub(super) markers_persisted: bool,
}

/// Surfaces a new save failure to the user and reports whether the save
/// succeeded, so the caller can mark its feature state persisted.
fn report_save<E: Display>(toast: &mut ToastState, label: &str, attempt: SaveAttempt<E>) -> bool {
    match attempt {
        SaveAttempt::Saved => return true,
        SaveAttempt::Failed {
            error,
            notify_user: true,
        } => toast.show(format!(
            "Could not save local {label} ({error}). Retrying in a few seconds."
        )),
        SaveAttempt::Idle
        | SaveAttempt::Deferred
        | SaveAttempt::Unavailable
        | SaveAttempt::Failed {
            notify_user: false, ..
        } => {}
    }
    false
}

impl TeamsApp {
    /// Initializes a store whose snapshot restores independently of every
    /// other store. An unreadable snapshot is kept intact (so sign-out can
    /// still remove it) but never overwritten during this session.
    fn init_simple_store<S, T, E: Display>(
        &mut self,
        slot: impl Fn(&mut LocalStores) -> &mut LocalFeatureStore<S>,
        noun: &str,
        restore_label: &str,
        for_profile: impl FnOnce(&AuthState) -> Option<S>,
        load: impl FnOnce(&S) -> Result<Option<T>, E>,
        restore: impl FnOnce(&mut Self, T),
    ) {
        if !self.local_persistence_enabled
            || !self.auth.is_authenticated()
            || !slot(&mut self.stores).begin_initialization()
        {
            return;
        }

        let Some(store) = for_profile(&self.auth) else {
            self.toast.show_error(format!(
                "Local {noun} storage is unavailable in this environment."
            ));
            return;
        };
        match load(&store) {
            Ok(Some(snapshot)) => {
                restore(self, snapshot);
                slot(&mut self.stores).allow_writes(store);
            }
            Ok(None) => {
                slot(&mut self.stores).allow_writes(store);
            }
            Err(error) => {
                slot(&mut self.stores).protect_existing_data(store);
                self.toast.show(format!(
                    "Could not restore local {restore_label} ({error}). This session will not overwrite it."
                ));
            }
        }
    }

    fn clear_simple_store<S, E: Display>(
        &mut self,
        slot: impl Fn(&mut LocalStores) -> &mut LocalFeatureStore<S>,
        noun: &str,
        clear: impl FnOnce(&S) -> Result<(), E>,
    ) {
        if let Some(store) = slot(&mut self.stores).take_for_cleanup()
            && let Err(error) = clear(&store)
        {
            self.queue_local_storage_notice(format!(
                "Could not clear the previous local {noun} cache ({error})."
            ));
        }
    }

    pub(super) fn ensure_local_presence_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.presence_store,
            "presence",
            "presence",
            |auth| LocalPresenceStore::for_profile(auth.local_profile()),
            LocalPresenceStore::load,
            |app, presence| app.auth.restore_local_presence(presence),
        );
    }

    pub(super) fn ensure_local_chat_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.chat_store,
            "chat",
            "chat data",
            |auth| LocalChatStore::for_profile(auth.local_profile()),
            LocalChatStore::load,
            |app, snapshot| {
                // Device-local choices have their own store and may already
                // be restored; a chat restore must not reset them.
                let prefs = app.chat.prefs();
                app.chat = ChatState::from_stored(snapshot);
                app.chat.restore_prefs(prefs);
            },
        );
    }

    pub(super) fn ensure_local_chat_prefs_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.chat_prefs_store,
            "chat preferences",
            "chat preferences",
            |auth| LocalChatPrefsStore::for_profile(auth.local_profile()),
            LocalChatPrefsStore::load,
            |app, prefs| app.chat.restore_prefs(prefs),
        );
    }

    pub(super) fn ensure_local_app_prefs_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.app_prefs_store,
            "app preferences",
            "app preferences",
            |auth| LocalAppPrefsStore::for_profile(auth.local_profile()),
            LocalAppPrefsStore::load,
            |app, prefs| {
                app.app_prefs = prefs;
                app.app_prefs_dirty = false;
            },
        );
    }

    pub(super) fn ensure_local_calendar_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.calendar_store,
            "calendar",
            "calendar data",
            |auth| LocalCalendarStore::for_profile(auth.local_profile()),
            LocalCalendarStore::load,
            |app, snapshot| app.workspace.restore_calendar(snapshot),
        );
    }

    pub(super) fn ensure_local_activity_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.activity_store,
            "activity",
            "activity data",
            |auth| LocalActivityStore::for_profile(auth.local_profile()),
            LocalActivityStore::load,
            |app, snapshot| app.workspace.restore_activity(snapshot),
        );
    }

    pub(super) fn ensure_local_teams_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.teams_store,
            "Teams",
            "Teams data",
            |auth| LocalTeamsStore::for_profile(auth.local_profile()),
            LocalTeamsStore::load,
            |app, snapshot| app.workspace.restore_teams(snapshot),
        );
    }

    /// Initializes the canonical discussion store independently from the old
    /// feature snapshots. A damaged legacy cache must not hide a valid shared
    /// discussion snapshot; source recovery is required only before importing
    /// or clearing that source's old overlay.
    pub(super) fn ensure_local_threads_store(&mut self) {
        if !self.local_persistence_enabled || !self.auth.is_authenticated() {
            return;
        }

        if self.stores.threads_store.begin_initialization() {
            let Some(store) = LocalThreadStore::for_profile(self.auth.local_profile()) else {
                self.toast
                    .show_error("Local discussion storage is unavailable in this environment.");
                return;
            };
            self.recover_local_threads_store(store);
        }

        if !self.stores.threads_store.is_writable() {
            return;
        }

        if self.thread_migration.markers_persisted {
            // These markers came from a successfully loaded or just-saved
            // canonical snapshot, so any lingering legacy records are a
            // recoverable post-save cleanup from an earlier run.
            self.clear_confirmed_legacy_thread_overlays();
            self.thread_migration.error_shown = false;
            return;
        }

        // A source that is already marked canonical need not recover again.
        // An unmarked source must be writable before we can import it; this
        // prevents an unreadable old snapshot from being acknowledged or
        // overwritten while still allowing the canonical overlay to render.
        let activity_ready = self.stores.activity_store.is_writable()
            || self.workspace.activity_thread_import_is_confirmed();
        let teams_ready = self.stores.teams_store.is_writable()
            || self.workspace.teams_thread_import_is_confirmed();
        if !activity_ready || !teams_ready {
            return;
        }

        match self.workspace.import_unmigrated_legacy_threads() {
            Ok(()) => self.thread_migration.error_shown = false,
            Err(error) if !self.thread_migration.error_shown => {
                self.thread_migration.error_shown = true;
                self.toast.show(format!(
                    "Could not migrate local discussion data ({error}). Existing feature caches were kept."
                ));
            }
            Err(_) => {}
        }
    }

    /// Clears only source overlays whose own snapshot was recovered safely.
    /// This may run after a canonical save or when a previously saved marker
    /// is loaded. It deliberately does nothing while migration is only
    /// in-memory, preserving the crash-safe save ordering.
    pub(super) fn clear_confirmed_legacy_thread_overlays(&mut self) {
        if !self.thread_migration.markers_persisted {
            return;
        }
        if self.stores.activity_store.is_writable() {
            self.workspace.clear_migrated_activity_legacy_threads();
        }
        if self.stores.teams_store.is_writable() {
            self.workspace.clear_migrated_teams_legacy_threads();
        }
    }

    /// Recovers the canonical store without making its visibility depend on
    /// the legacy source adapters. This keeps a valid shared overlay usable
    /// even when an old source file is damaged.
    pub(super) fn recover_local_threads_store(&mut self, store: LocalThreadStore) {
        match store.load() {
            Ok(Some(snapshot)) => {
                self.workspace.restore_threads(snapshot);
                self.thread_migration.markers_persisted =
                    self.workspace.legacy_thread_imports_are_confirmed();
                self.stores.threads_store.allow_writes(store);
            }
            Ok(None) => {
                self.thread_migration.markers_persisted = false;
                self.stores.threads_store.allow_writes(store);
            }
            Err(error) => {
                self.thread_migration.markers_persisted = false;
                // Keep an unreadable canonical snapshot intact. When both
                // old source snapshots recovered safely, their overlays can
                // still be shown for this session without clearing or
                // overwriting either file.
                if self.stores.activity_store.is_writable()
                    && self.stores.teams_store.is_writable()
                    && let Err(import_error) = self.workspace.import_unmigrated_legacy_threads()
                {
                    self.toast.show(format!(
                        "Could not recover local discussion overlays ({import_error})."
                    ));
                }
                self.stores.threads_store.protect_existing_data(store);
                self.toast.show(format!(
                    "Could not restore local discussion data ({error}). This session will not overwrite it."
                ));
            }
        }
    }

    pub(super) fn ensure_local_calls_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.calls_store,
            "Calls",
            "Calls data",
            |auth| LocalCallsStore::for_profile(auth.local_profile()),
            LocalCallsStore::load,
            |app, snapshot| app.workspace.restore_calls(snapshot),
        );
    }

    pub(super) fn ensure_local_files_store(&mut self) {
        self.init_simple_store(
            |stores| &mut stores.files_store,
            "Files",
            "Files data",
            |auth| LocalFilesStore::for_profile(auth.local_profile()),
            LocalFilesStore::load,
            |app, snapshot| app.workspace.restore_files(snapshot),
        );
    }

    pub(super) fn flush_local_chat(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, chat) = (&mut self.stores.chat_store, &mut self.chat);
            store.save_if_due(
                ctx,
                chat.is_persistence_dirty(),
                || chat.stored(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "chat data", attempt) {
            self.chat.mark_persisted();
        }
    }

    pub(super) fn flush_local_chat_prefs(&mut self, ctx: &egui::Context) {
        let prefs = self.chat.prefs();
        let attempt = self.stores.chat_prefs_store.save_if_due(
            ctx,
            self.chat.is_prefs_dirty(),
            || prefs,
            |store, snapshot| store.save(snapshot),
        );
        if report_save(&mut self.toast, "chat preferences", attempt) {
            self.chat.mark_prefs_persisted();
        }
    }

    pub(super) fn flush_local_app_prefs(&mut self, ctx: &egui::Context) {
        let prefs = self.app_prefs;
        let attempt = self.stores.app_prefs_store.save_if_due(
            ctx,
            self.app_prefs_dirty,
            || prefs,
            |store, snapshot| store.save(*snapshot),
        );
        if report_save(&mut self.toast, "app preferences", attempt) {
            self.app_prefs_dirty = false;
        }
    }

    pub(super) fn flush_local_presence(&mut self, ctx: &egui::Context) {
        let presence = self.auth.local_presence();
        let attempt = self.stores.presence_store.save_if_due(
            ctx,
            self.auth.is_presence_dirty(),
            || presence,
            |store, snapshot| store.save(*snapshot),
        );
        if report_save(&mut self.toast, "presence", attempt) {
            self.auth.mark_presence_persisted();
        }
    }

    pub(super) fn flush_local_calendar(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.calendar_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_calendar_persistence_dirty(),
                || workspace.calendar_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "calendar data", attempt) {
            self.workspace.mark_calendar_persisted();
        }
    }

    pub(super) fn flush_local_activity(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.activity_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_activity_persistence_dirty(),
                || workspace.activity_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "activity data", attempt) {
            self.workspace.mark_activity_persisted();
        }
    }

    pub(super) fn flush_local_teams(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.teams_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_teams_persistence_dirty(),
                || workspace.teams_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "Teams data", attempt) {
            self.workspace.mark_teams_persisted();
        }
    }

    pub(super) fn flush_local_threads(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.threads_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_threads_persistence_dirty(),
                || workspace.threads_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "discussion data", attempt) {
            self.workspace.mark_threads_persisted();
            // The just-saved snapshot contains the migration markers and
            // imported data, so it is now safe to clear old overlays.
            self.thread_migration.markers_persisted =
                self.workspace.legacy_thread_imports_are_confirmed();
            self.clear_confirmed_legacy_thread_overlays();
            self.thread_migration.error_shown = false;
        }
    }

    pub(super) fn flush_local_calls(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.calls_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_calls_persistence_dirty(),
                || workspace.calls_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "Calls data", attempt) {
            self.workspace.mark_calls_persisted();
        }
    }

    pub(super) fn flush_local_files(&mut self, ctx: &egui::Context) {
        let attempt = {
            let (store, workspace) = (&mut self.stores.files_store, &mut self.workspace);
            store.save_if_due(
                ctx,
                workspace.is_files_persistence_dirty(),
                || workspace.files_snapshot(),
                |store, snapshot| store.save(snapshot),
            )
        };
        if report_save(&mut self.toast, "Files data", attempt) {
            self.workspace.mark_files_persisted();
        }
    }

    /// Writes every dirty store now, bypassing the save debounce, and returns
    /// a message for each store whose latest save failed. Used on exit so the
    /// last edits are not lost to the debounce window.
    pub(super) fn flush_all_local_stores_now(&mut self, ctx: &egui::Context) -> Vec<String> {
        let stores = &mut self.stores;
        stores.presence_store.force_next_save();
        stores.chat_store.force_next_save();
        stores.chat_prefs_store.force_next_save();
        stores.app_prefs_store.force_next_save();
        stores.calendar_store.force_next_save();
        stores.activity_store.force_next_save();
        stores.teams_store.force_next_save();
        stores.threads_store.force_next_save();
        stores.calls_store.force_next_save();
        stores.files_store.force_next_save();
        self.flush_local_presence(ctx);
        self.flush_local_chat(ctx);
        self.flush_local_chat_prefs(ctx);
        self.flush_local_app_prefs(ctx);
        self.flush_local_calendar(ctx);
        self.flush_local_activity(ctx);
        self.flush_local_teams(ctx);
        self.flush_local_threads(ctx);
        self.flush_local_calls(ctx);
        self.flush_local_files(ctx);
        self.local_storage_warnings()
    }

    /// Persistent, non-dismissible warnings for stores whose latest save
    /// failed (including a snapshot that outgrew its size limit). A warning
    /// disappears as soon as a later save succeeds.
    pub(super) fn local_storage_warnings(&self) -> Vec<String> {
        let stores = &self.stores;
        [
            ("presence", stores.presence_store.failure()),
            ("chat data", stores.chat_store.failure()),
            ("chat preferences", stores.chat_prefs_store.failure()),
            ("app preferences", stores.app_prefs_store.failure()),
            ("calendar data", stores.calendar_store.failure()),
            ("activity data", stores.activity_store.failure()),
            ("Teams data", stores.teams_store.failure()),
            ("discussion data", stores.threads_store.failure()),
            ("Calls data", stores.calls_store.failure()),
            ("Files data", stores.files_store.failure()),
        ]
        .into_iter()
        .filter_map(|(label, failure)| {
            failure.map(|reason| {
                format!(
                    "Could not save local {label} ({reason}). Recent changes are not stored on this device yet."
                )
            })
        })
        .collect()
    }

    pub(super) fn clear_local_chat_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.chat_store,
            "chat",
            LocalChatStore::clear,
        );
    }

    pub(super) fn clear_local_chat_prefs_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.chat_prefs_store,
            "chat preferences",
            LocalChatPrefsStore::clear,
        );
    }

    pub(super) fn clear_local_app_prefs_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.app_prefs_store,
            "app preferences",
            LocalAppPrefsStore::clear,
        );
    }

    pub(super) fn clear_local_presence_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.presence_store,
            "presence",
            LocalPresenceStore::clear,
        );
    }

    pub(super) fn clear_local_calendar_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.calendar_store,
            "calendar",
            LocalCalendarStore::clear,
        );
    }

    pub(super) fn clear_local_activity_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.activity_store,
            "activity",
            LocalActivityStore::clear,
        );
    }

    pub(super) fn clear_local_teams_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.teams_store,
            "Teams",
            LocalTeamsStore::clear,
        );
    }

    pub(super) fn clear_local_threads_store(&mut self) {
        if let Some(store) = self.stores.threads_store.take_for_cleanup()
            && let Err(error) = store.clear()
        {
            self.queue_local_storage_notice(format!(
                "Could not clear the previous local discussion cache ({error})."
            ));
        }
        self.thread_migration.markers_persisted = false;
        self.thread_migration.error_shown = false;
    }

    pub(super) fn clear_local_calls_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.calls_store,
            "Calls",
            LocalCallsStore::clear,
        );
    }

    pub(super) fn clear_local_files_store(&mut self) {
        self.clear_simple_store(
            |stores| &mut stores.files_store,
            "Files",
            LocalFilesStore::clear,
        );
    }
}
