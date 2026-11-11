use crate::data::shared_files::shared_file_for_name;
use crate::data::team_catalog::team_for_channel;
use crate::features::activity::{ActivityAction, ActivityState, StoredActivity};
use crate::features::calendar::{
    CalendarAction, CalendarState, RemoteCalendarDirectory, StoredCalendar,
};
use crate::features::calls::{CallMode, CallsState, StoredCalls};
use crate::features::files::{FilesState, RemoteFilePreview, RemoteFilesDirectory, StoredFiles};
use crate::features::search::workspace::{
    FileViewTarget, WorkspaceSearchResult, WorkspaceSearchTarget,
};
use crate::features::teams::{
    self, RemoteChannelFilesDirectory, RemoteChannelMessagesDirectory,
    RemoteChannelPostSubmissionId, RemoteChannelRepliesDirectory, RemoteChannelReplySubmissionId,
    RemoteTeamsDirectory, StoredTeams, TeamsState,
};
use crate::features::threads::{
    LegacyThreadMutation, StoredThreads, ThreadChange, ThreadError, ThreadStore, ThreadTarget,
};
use crate::integrations::{
    ProviderBrowserUrl, ProviderResourceRef, WorkspaceCommand, WorkspaceIntentId,
};
use crate::shared::model::Section;
use crate::shared::reaction::ReactionKind;
use crate::ui::toast::{ToastState, UndoKind};
use eframe::egui;

/// Coordinates state shared by the non-chat workspace surfaces.
///
/// Feature-local state remains owned by its feature module. This coordinator
/// owns the small set of lifecycle, navigation, search, and mutual-exclusion
/// rules that span those modules.
#[derive(Default)]
pub(crate) struct WorkspaceState {
    activity: ActivityState,
    calls: CallsState,
    files: FilesState,
    calendar: CalendarState,
    teams: TeamsState,
    threads: ThreadStore,
    navigation_request: Option<Section>,
    pending_transport_commands: Vec<WorkspaceCommand>,
    /// Remote channel posts retain their feature-local submission key until
    /// the app has enqueued the provider-neutral command and can bind its
    /// controller-owned intent ID. Keeping this separate from generic/local
    /// commands prevents an opaque remote composer from being mistaken for a
    /// durable local post.
    pending_remote_channel_posts: Vec<PendingRemoteChannelPost>,
    /// Remote channel replies have the same opaque app handoff as root posts,
    /// but must never share the selected-root reply *read* slot below. A
    /// queued write may target an older root while the user reads another.
    pending_remote_channel_reply_writes: Vec<PendingRemoteChannelReply>,
    /// Remote root-post reactions remain route-complete but have no local
    /// optimistic overlay. The app binds each handoff to a controller intent
    /// so a known rejection unlocks only that exact feature gate.
    pending_remote_channel_post_reactions: Vec<PendingRemoteChannelPostReaction>,
    /// Selected-root reply reactions use a distinct complete route. Keeping
    /// them separate from root-post reactions prevents a terminal outcome or
    /// refresh for one feed from affecting the other.
    pending_remote_channel_reply_reactions: Vec<PendingRemoteChannelReplyReaction>,
    /// The newest selected remote channel feed to query. This is not a
    /// mutation command and never becomes part of a durable local snapshot.
    pending_remote_channel_messages: Option<(ProviderResourceRef, ProviderResourceRef)>,
    /// The newest selected remote channel backing-folder metadata view to
    /// query. This remains independent of both OneDrive root and root posts.
    pending_remote_channel_files: Option<(ProviderResourceRef, ProviderResourceRef)>,
    /// The newest selected remote root-post reply feed to query. Like the
    /// root-post feed, it is a session-only read rather than a command.
    pending_remote_channel_replies: Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )>,
    /// A feature transition discarded its selected-root-post reply state.
    /// The app drains this before it stages replacement reads so the transport
    /// can cancel only obsolete `ChannelReplies` work.
    cancel_remote_channel_replies_requested: bool,
    /// A feature transition discarded its selected-channel file state. The
    /// app drains this before staging a replacement so only `ChannelFiles`
    /// work is cancelled.
    cancel_remote_channel_files_requested: bool,
}

/// A session-only handoff from the Teams feature to the app-owned transport
/// controller. The feature creates the opaque submission key; only the app
/// may turn its provider references into `WorkspaceCommand::SendChannelPost`.
struct PendingRemoteChannelPost {
    submission: RemoteChannelPostSubmissionId,
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    body: String,
}

/// A session-only handoff for one text-only reply to a selected remote root
/// post. This intentionally remains distinct from `pending_remote_channel_replies`,
/// which is only a selected-root `ChannelReplies` read request.
struct PendingRemoteChannelReply {
    submission: RemoteChannelReplySubmissionId,
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: String,
}

/// A session-only handoff for one exact root-post reaction. Unlike the local
/// generic reaction intent, every provider route segment remains opaque and
/// attached here until the app queues the Microsoft 365 command.
struct PendingRemoteChannelPostReaction {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    reaction: ReactionKind,
    selected: bool,
}

/// A session-only handoff for one exact selected-root reply reaction. Every
/// Graph route segment remains opaque until the app queues the provider-neutral
/// command and binds its controller intent back to the feature gate.
struct PendingRemoteChannelReplyReaction {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    reply: ProviderResourceRef,
    reaction: ReactionKind,
    selected: bool,
}

impl WorkspaceState {
    /// Restores Teams-owned durable mutations while keeping workspace
    /// navigation and cross-surface state fresh for this local sign-in.
    pub(crate) fn restore_teams(&mut self, snapshot: StoredTeams) {
        self.teams = TeamsState::from_stored(snapshot);
    }

    pub(crate) const fn is_teams_persistence_dirty(&self) -> bool {
        self.teams.is_persistence_dirty()
    }

    pub(crate) fn teams_snapshot(&self) -> StoredTeams {
        self.teams.stored()
    }

    pub(crate) fn mark_teams_persisted(&mut self) {
        self.teams.mark_persisted();
    }

    /// Replaces the session-only Microsoft 365 Teams directory. The feature
    /// owns its selection and rendering state; this coordinator deliberately
    /// never merges remote rows into the persisted local Teams snapshot.
    pub(crate) fn replace_remote_teams_directory(&mut self, directory: RemoteTeamsDirectory) {
        self.teams.replace_remote_directory(directory);
    }

    /// Clears only the remote Teams projection during an account/profile
    /// boundary. Local preview teams, posts, and durable preferences remain
    /// available after disconnect.
    pub(crate) fn clear_remote_teams_directory(&mut self) {
        self.teams.clear_remote_directory();
        self.pending_remote_channel_posts.clear();
        self.pending_remote_channel_reply_writes.clear();
        self.pending_remote_channel_post_reactions.clear();
        self.pending_remote_channel_reply_reactions.clear();
        self.pending_remote_channel_messages = None;
        self.discard_pending_remote_channel_files();
        self.discard_pending_remote_channel_replies();
    }

    /// Clears stale remote Teams views after an unexpected adapter/worker
    /// loss while preserving an already-bound root-post or root-reply composer
    /// long enough for its queued terminal delivery outcome to restore or mark
    /// its text.
    /// This is deliberately narrower than [`Self::clear_remote_teams_directory`]:
    /// explicit disconnects and profile/account boundaries must still discard
    /// every remote draft.
    pub(crate) fn clear_remote_teams_directory_preserving_delivery_recovery(&mut self) {
        self.teams
            .clear_remote_directory_preserving_delivery_recovery();
        self.pending_remote_channel_posts.clear();
        self.pending_remote_channel_reply_writes.clear();
        self.pending_remote_channel_post_reactions.clear();
        self.pending_remote_channel_reply_reactions.clear();
        self.pending_remote_channel_messages = None;
        self.discard_pending_remote_channel_files();
        self.discard_pending_remote_channel_replies();
    }

    /// Applies a selected remote channel's session-only root-post view. The
    /// Teams feature rejects a projection if the user selected another opaque
    /// channel while its worker was in flight.
    pub(crate) fn replace_remote_channel_messages(
        &mut self,
        directory: RemoteChannelMessagesDirectory,
    ) -> bool {
        self.teams.replace_remote_channel_messages(directory)
    }

    /// Applies the selected channel's session-only backing-folder metadata.
    /// The Teams feature rejects it when either opaque selection target is no
    /// longer current, so late provider work cannot become durable state.
    pub(crate) fn replace_remote_channel_files(
        &mut self,
        directory: RemoteChannelFilesDirectory,
    ) -> bool {
        self.teams.replace_remote_channel_files(directory)
    }

    /// Applies one selected root post's session-only reply view. The Teams
    /// feature verifies all three opaque selection targets before it replaces
    /// visible state, so a late response never becomes local persistence.
    pub(crate) fn replace_remote_channel_replies(
        &mut self,
        directory: RemoteChannelRepliesDirectory,
    ) -> bool {
        self.teams.replace_remote_channel_replies(directory)
    }

    /// Restores the canonical discussion overlay after feature-owned legacy
    /// snapshots have been loaded. Feature presentation state stays fresh.
    pub(crate) fn restore_threads(&mut self, snapshot: StoredThreads) {
        self.threads = ThreadStore::from_stored(snapshot);
    }

    pub(crate) const fn is_threads_persistence_dirty(&self) -> bool {
        self.threads.is_persistence_dirty()
    }

    pub(crate) fn threads_snapshot(&self) -> StoredThreads {
        self.threads.stored()
    }

    pub(crate) fn mark_threads_persisted(&mut self) {
        self.threads.mark_persisted();
    }

    pub(crate) const fn legacy_thread_imports_are_confirmed(&self) -> bool {
        self.threads.activity_v1_imported() && self.threads.teams_v1_imported()
    }

    /// Reports whether the Activity source overlay has already been copied
    /// into a canonical snapshot. The app uses this to recover the canonical
    /// overlay even when a now-unneeded legacy file cannot be read.
    pub(crate) const fn activity_thread_import_is_confirmed(&self) -> bool {
        self.threads.activity_v1_imported()
    }

    /// See [`Self::activity_thread_import_is_confirmed`] for why this source
    /// marker is exposed separately from the all-sources convenience method.
    pub(crate) const fn teams_thread_import_is_confirmed(&self) -> bool {
        self.threads.teams_v1_imported()
    }

    /// Imports source-owned v1 discussion overlays into the canonical store.
    ///
    /// Each source is flagged only after its whole batch has been accepted by
    /// the transactional thread importer. The app must atomically save the
    /// canonical snapshot before clearing either source's old overlay.
    pub(crate) fn import_unmigrated_legacy_threads(&mut self) -> Result<(), ThreadError> {
        if !self.threads.activity_v1_imported() {
            let legacy = self.activity.legacy_discussion();
            let mut mutations = Vec::new();
            for (item_key, replies) in legacy.replies_by_item {
                let Some(thread) =
                    crate::features::activity::legacy_thread_id_for_item_key(&item_key)
                else {
                    continue;
                };
                mutations.extend(replies.into_iter().map(|body| LegacyThreadMutation::Reply {
                    thread: thread.clone(),
                    body,
                }));
            }
            for (target_key, selected_icons) in legacy.selected_reactions_by_target {
                let Some((thread, target, _)) =
                    crate::features::activity::legacy_reaction_target(&target_key)
                else {
                    continue;
                };
                mutations.push(LegacyThreadMutation::SelectedReactions {
                    thread,
                    target,
                    selected_icons,
                });
            }
            self.threads.import_legacy(mutations)?;
            self.threads.mark_activity_v1_imported()?;
        }

        if !self.threads.teams_v1_imported() {
            let legacy = self.teams.legacy_discussion();
            let mut mutations = Vec::new();
            if !legacy.is_empty() {
                for (channel, channel_discussion) in legacy.channels {
                    for (post_key, replies) in channel_discussion.replies_by_post {
                        let Some(thread) = self
                            .teams
                            .legacy_thread_id_for_channel_post(&channel, &post_key)
                        else {
                            continue;
                        };
                        mutations.extend(replies.into_iter().map(|body| {
                            LegacyThreadMutation::Reply {
                                thread: thread.clone(),
                                body,
                            }
                        }));
                    }
                    for (target_key, selected_icons) in
                        channel_discussion.selected_reactions_by_target
                    {
                        let Some((thread, target, _)) =
                            self.teams.legacy_reaction_target(&channel, &target_key)
                        else {
                            continue;
                        };
                        mutations.push(LegacyThreadMutation::SelectedReactions {
                            thread,
                            target,
                            selected_icons,
                        });
                    }
                }
            }
            self.threads.import_legacy(mutations)?;
            self.threads.mark_teams_v1_imported()?;
        }
        Ok(())
    }

    /// Clears Activity's old overlay only after Activity's source marker is
    /// known to be durable in the canonical snapshot.
    pub(crate) fn clear_migrated_activity_legacy_threads(&mut self) {
        if self.threads.activity_v1_imported() {
            self.activity.clear_legacy_discussion();
        }
    }

    /// Clears Teams' old overlay only after Teams' source marker is known to
    /// be durable in the canonical snapshot.
    pub(crate) fn clear_migrated_teams_legacy_threads(&mut self) {
        if self.threads.teams_v1_imported() {
            self.teams.clear_legacy_discussion();
        }
    }

    /// Restores only Activity-owned durable mutations. Navigation and other
    /// cross-surface session state remain fresh for the current sign-in.
    pub(crate) fn restore_activity(&mut self, snapshot: StoredActivity) {
        self.activity = ActivityState::from_stored(snapshot);
    }

    pub(crate) const fn is_activity_persistence_dirty(&self) -> bool {
        self.activity.is_persistence_dirty()
    }

    pub(crate) fn activity_snapshot(&self) -> StoredActivity {
        self.activity.stored()
    }

    pub(crate) fn mark_activity_persisted(&mut self) {
        self.activity.mark_persisted();
    }

    /// Restores only durable Calls preferences and completed local history.
    /// Active calls, dial targets, filters, and dialogs remain session-only.
    pub(crate) fn restore_calls(&mut self, snapshot: StoredCalls) {
        self.calls = CallsState::from_stored(snapshot);
    }

    pub(crate) const fn is_calls_persistence_dirty(&self) -> bool {
        self.calls.is_persistence_dirty()
    }

    pub(crate) fn calls_snapshot(&self) -> StoredCalls {
        self.calls.stored()
    }

    pub(crate) fn mark_calls_persisted(&mut self) {
        self.calls.mark_persisted();
    }

    /// Restores only local file-list metadata. Search, selection, previews,
    /// and bundled shared assets remain fresh for this session.
    pub(crate) fn restore_files(&mut self, snapshot: StoredFiles) {
        self.files = FilesState::from_stored(snapshot);
    }

    pub(crate) const fn is_files_persistence_dirty(&self) -> bool {
        self.files.is_persistence_dirty()
    }

    pub(crate) fn files_snapshot(&self) -> StoredFiles {
        self.files.stored()
    }

    pub(crate) fn mark_files_persisted(&mut self) {
        self.files.mark_persisted();
    }

    /// Replaces the session-only Microsoft 365 OneDrive-root metadata view.
    /// It is intentionally not merged with user-chosen local paths or the
    /// durable Files snapshot.
    pub(crate) fn replace_remote_files_directory(&mut self, directory: RemoteFilesDirectory) {
        self.files.replace_remote_files_directory(directory);
    }

    /// Clears the remote OneDrive view at an account/profile boundary while
    /// preserving local Files entries and their persistence lifecycle.
    pub(crate) fn clear_remote_files_directory(&mut self) {
        self.files.clear_remote_files_directory();
    }

    /// Applies one normalized preview only if Files still owns the exact
    /// selected OneDrive file. A stale remote worker result never becomes a
    /// local preview or reopens a cleared account surface.
    pub(crate) fn apply_remote_file_preview(&mut self, preview: RemoteFilePreview) -> bool {
        self.files.apply_remote_file_preview(preview)
    }

    /// Lets the app-owned egui context discard loader and texture cache state
    /// after the Files feature has dropped a remote image preview. This is
    /// intentionally separate from the feature's provider/session lifecycle:
    /// it must still run when Files is no longer the visible section.
    pub(crate) fn forget_discarded_remote_file_preview_images(&mut self, ctx: &egui::Context) {
        self.files.forget_discarded_remote_preview_images(ctx);
    }

    /// Restores only calendar-owned durable data. All cross-surface and UI
    /// session state remains fresh for the current local sign-in.
    pub(crate) fn restore_calendar(&mut self, snapshot: StoredCalendar) {
        self.calendar = CalendarState::from_stored(snapshot);
    }

    pub(crate) const fn is_calendar_persistence_dirty(&self) -> bool {
        self.calendar.is_persistence_dirty()
    }

    pub(crate) fn calendar_snapshot(&self) -> StoredCalendar {
        self.calendar.stored()
    }

    pub(crate) fn mark_calendar_persisted(&mut self) {
        self.calendar.mark_persisted();
    }

    /// Replaces the session-only Microsoft 365 calendar view without ever
    /// adding provider events to local calendar storage or mutation state.
    pub(crate) fn replace_remote_calendar_directory(&mut self, directory: RemoteCalendarDirectory) {
        self.calendar.replace_remote_directory(directory);
    }

    /// Clears the remote calendar projection at an account/profile boundary
    /// while retaining local-preview events and preferences.
    pub(crate) fn clear_remote_calendar_directory(&mut self) {
        self.calendar.clear_remote_directory();
    }

    /// Drains direct user requests to open an existing, normalized Microsoft
    /// 365 event meeting link. This deliberately bypasses the workspace
    /// transport: opening a browser is not a Graph mutation or a local call.
    pub(crate) fn take_remote_meeting_join_links(&mut self) -> Vec<String> {
        self.calendar.take_remote_meeting_join_links()
    }

    /// Drains direct user requests to open an integration-normalized OneDrive
    /// item in the system browser. This stays outside workspace transport and
    /// local Files state: opening a provider URL is neither a Graph mutation
    /// nor a local file operation.
    pub(crate) fn take_remote_file_open_links(&mut self) -> Vec<ProviderBrowserUrl> {
        self.files.take_remote_file_open_links()
    }

    /// Drains an explicit, opaque OneDrive content selection to the app-owned
    /// remote transport. This remains a selected read rather than a command,
    /// browser handoff, or local Files operation.
    pub(crate) fn take_remote_file_preview_request(&mut self) -> Option<ProviderResourceRef> {
        self.files.take_remote_file_preview_request()
    }

    /// Drains a feature-owned request to cancel only obsolete selected-file
    /// preview work. It cannot affect account-wide OneDrive metadata or any
    /// chat/channel synchronization.
    pub(crate) fn take_remote_file_preview_cancellation_requested(&mut self) -> bool {
        self.files.take_remote_file_preview_cancellation_requested()
    }

    /// Drains direct user requests to open one current selected-channel item
    /// in the system browser. This remains a session-only browser handoff,
    /// distinct from the OneDrive root view, its bounded provider read, and
    /// every workspace transport command.
    pub(crate) fn take_remote_channel_file_open_links(&mut self) -> Vec<ProviderBrowserUrl> {
        self.teams.take_remote_channel_file_open_links()
    }

    pub(crate) fn has_keyboard_modal(&self, section: Section) -> bool {
        match section {
            Section::Teams => self.teams.has_keyboard_modal(),
            Section::Calendar => self.calendar.has_keyboard_modal(),
            Section::Calls => self.calls.has_keyboard_modal(),
            Section::Files => self.files.has_keyboard_modal(),
            Section::Activity | Section::Chat => false,
        }
    }

    pub(crate) fn dismiss_section_transients(&mut self, section: Section) {
        match section {
            Section::Activity => {}
            Section::Teams => self.teams.dismiss_contextual_overlays(),
            Section::Calendar => self.calendar.dismiss_contextual_overlays(),
            Section::Calls => self.calls.dismiss_contextual_overlays(),
            Section::Files => self.files.dismiss_contextual_overlays(),
            Section::Chat => {}
        }
    }

    pub(crate) fn search_results(&self, query: &str) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        let mut results = self.activity.search_results(&self.threads, &query);
        results.extend(self.calendar.search_results(&query));
        results.extend(self.calls.search_results(&query));
        results.extend(self.files.search_results(&query));
        results.extend(self.teams.search_results(&self.threads, &query));
        results
    }

    pub(crate) fn select_search_result(&mut self, target: WorkspaceSearchTarget) -> Section {
        match target {
            WorkspaceSearchTarget::Activity(index) => {
                let _ = self.activity.select_search_result(index);
                Section::Activity
            }
            WorkspaceSearchTarget::TeamChannel(channel) => {
                self.select_local_team_channel(&channel);
                Section::Teams
            }
            WorkspaceSearchTarget::TeamPost { channel, post_key } => {
                self.select_local_team_channel(&channel);
                let _ = self.teams.open_selected_post_thread(&post_key);
                Section::Teams
            }
            WorkspaceSearchTarget::CalendarEvent {
                event_key,
                title,
                week_offset,
                day,
                time,
            } => {
                self.calendar
                    .select_search_result(event_key, title, week_offset, day, time);
                Section::Calendar
            }
            WorkspaceSearchTarget::File {
                name,
                source,
                when,
                kind,
                view,
            } => {
                self.files
                    .select_search_result(name, source, when, kind, view);
                Section::Files
            }
            WorkspaceSearchTarget::CallContact(contact) => {
                self.calls.select_search_result(contact);
                Section::Calls
            }
        }
    }

    pub(crate) fn start_chat_call(&mut self, participant: &str, mode: CallMode) -> Option<String> {
        if self.calendar.has_active_meeting() {
            return None;
        }
        self.calls.start_call(participant, mode)
    }

    pub(crate) fn start_channel_meeting(&mut self) -> bool {
        if !self.calendar.start_channel_meeting(
            self.teams.selected_channel_name(),
            self.calls.has_active_call(),
        ) {
            return false;
        }
        self.request_navigation(Section::Calendar);
        true
    }

    pub(crate) fn has_active_meeting(&self) -> bool {
        self.calendar.has_active_meeting()
    }

    pub(crate) fn has_active_call(&self) -> bool {
        self.calls.has_active_call()
    }

    pub(crate) fn open_shared_preview(&mut self, name: &str) -> bool {
        let Some(file) = shared_file_for_name(name) else {
            return false;
        };
        let channel = self.teams.selected_channel_name();
        if !file.team_channels.contains(&channel) {
            return false;
        }
        self.files.select_search_result(
            file.name.into(),
            file.source.into(),
            file.detail.into(),
            file.kind.label().into(),
            FileViewTarget::Shared,
        );
        self.request_navigation(Section::Files);
        true
    }

    /// Unread local activity items, for the rail badge.
    pub(crate) fn activity_unread_count(&self) -> usize {
        self.activity.unread_count()
    }

    /// Reverses the last local delete of `kind` that this workspace owns and
    /// returns the toast text. Chat messages are handled by the chat feature.
    pub(crate) fn undo_local_delete(&mut self, kind: UndoKind) -> Option<(bool, &'static str)> {
        match kind {
            UndoKind::ChannelPost => {
                Some(if self.teams.undo_delete_local_post(&mut self.threads) {
                    (true, "Restored the channel post")
                } else {
                    (false, "Couldn't restore that channel post.")
                })
            }
            UndoKind::CallHistory => Some(if self.calls.undo_remove_history_entry() {
                (true, "Restored the call history entry")
            } else {
                (false, "Couldn't restore that call history entry.")
            }),
            UndoKind::SpeedDial => Some(if self.calls.undo_remove_speed_dial() {
                (true, "Restored the speed dial contact")
            } else {
                (false, "Couldn't restore that speed dial contact.")
            }),
            UndoKind::ChatMessage => None,
        }
    }

    pub(crate) fn draw_transient_overlays(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        self.calls.draw_active_call(ctx, toast);
        self.calendar.draw_active_meeting(ctx, toast);
    }

    /// Renders the selected workspace sidebar through the state that owns all
    /// cross-surface lifecycle and navigation invariants.
    pub(crate) fn draw_sidebar(
        &mut self,
        ui: &mut egui::Ui,
        section: Section,
        toast: &mut ToastState,
    ) {
        match section {
            Section::Activity => self.activity.draw_sidebar(ui, toast),
            Section::Teams => {
                let can_start_meeting = !self.has_active_call() && !self.has_active_meeting();
                if let Some(action) =
                    teams::draw_sidebar(ui, &mut self.teams, can_start_meeting, toast)
                {
                    self.run_teams_action(action, toast);
                }
            }
            Section::Calendar => self.calendar.draw_sidebar(ui, toast),
            Section::Calls => {
                self.calls
                    .draw_sidebar(ui, toast, self.calendar.has_active_meeting())
            }
            Section::Files => self.files.draw_sidebar(ui),
            Section::Chat => {}
        }
    }

    /// Renders the selected workspace page through the same state owner as its
    /// sidebar, so callers cannot separate presentation from session state.
    pub(crate) fn draw_page(
        &mut self,
        ui: &mut egui::Ui,
        section: Section,
        toast: &mut ToastState,
    ) {
        match section {
            Section::Activity => {
                if let Some(action) = self.activity.draw_page(ui, toast, &mut self.threads) {
                    self.run_activity_action(action, toast);
                }
            }
            Section::Teams => {
                let can_start_meeting = !self.has_active_call() && !self.has_active_meeting();
                if let Some(action) = teams::draw_page(
                    ui,
                    &mut self.teams,
                    &mut self.threads,
                    can_start_meeting,
                    toast,
                ) {
                    self.run_teams_action(action, toast);
                }
            }
            Section::Calendar => self
                .calendar
                .draw_page(ui, toast, self.calls.has_active_call()),
            Section::Calls => self
                .calls
                .draw_page(ui, toast, self.calendar.has_active_meeting()),
            Section::Files => self.files.draw_page(ui, toast),
            Section::Chat => {}
        }
    }

    fn run_teams_action(&mut self, action: teams::TeamsAction, toast: &mut ToastState) {
        match action {
            teams::TeamsAction::StartMeeting => {
                toast.show(if self.start_channel_meeting() {
                    "Started a local channel meeting"
                } else {
                    "End the active local call or meeting before starting another."
                });
            }
            teams::TeamsAction::OpenSharedPreview(name) => {
                let _ = self.open_shared_preview(&name);
            }
            teams::TeamsAction::ChannelPostSubmitted { channel, body } => {
                let team = team_for_channel(&channel)
                    .map(|team| ProviderResourceRef::local_preview(team.name))
                    .unwrap_or_else(|| ProviderResourceRef::local_preview(channel.clone()));
                self.pending_transport_commands
                    .push(WorkspaceCommand::SendChannelPost {
                        team,
                        channel: ProviderResourceRef::local_preview(channel),
                        body,
                    });
            }
            teams::TeamsAction::SendRemoteChannelPost {
                submission,
                team,
                channel,
                body,
            } => {
                // This is a feature-selected, text-only remote mutation. It
                // stays out of local Teams persistence and gets its opaque
                // controller intent only when the app stages it below.
                self.pending_remote_channel_posts
                    .push(PendingRemoteChannelPost {
                        submission,
                        team,
                        channel,
                        body,
                    });
            }
            teams::TeamsAction::SendRemoteChannelReply {
                submission,
                team,
                channel,
                post,
                body,
            } => {
                // Preserve the provider-issued root target through the
                // app-owned handoff. This is a write queue, not the selected
                // root's bounded reply-read slot.
                self.pending_remote_channel_reply_writes
                    .push(PendingRemoteChannelReply {
                        submission,
                        team,
                        channel,
                        post,
                        body,
                    });
            }
            teams::TeamsAction::SetRemoteChannelPostReaction {
                team,
                channel,
                post,
                reaction,
                selected,
            } => {
                // This exact route stays isolated from the local generic
                // reaction projection. The app assigns an opaque intent ID
                // before it can resolve a known rejection back to the
                // feature-owned pending gate.
                self.pending_remote_channel_post_reactions
                    .push(PendingRemoteChannelPostReaction {
                        team,
                        channel,
                        post,
                        reaction,
                        selected,
                    });
            }
            teams::TeamsAction::SetRemoteChannelReplyReaction {
                team,
                channel,
                post,
                reply,
                reaction,
                selected,
            } => {
                // Root-reply reactions have an additional opaque route
                // segment. Do not fold them into root-post reactions or a
                // generic/local reaction command: the app must bind all four
                // provider targets to the feature-owned pending gate.
                self.pending_remote_channel_reply_reactions.push(
                    PendingRemoteChannelReplyReaction {
                        team,
                        channel,
                        post,
                        reply,
                        reaction,
                        selected,
                    },
                );
            }
            teams::TeamsAction::RemoteChannelSelected { team, channel } => {
                // Only the newest remote selection matters. The app-owned
                // controller turns this session-only query into a bounded
                // background poll; it is not a provider mutation command.
                // The same action also represents an explicit refresh. Keep
                // an in-flight selected-root reply read only when this exact
                // channel still owns that root; an initial or changed channel
                // selection must conservatively cancel any old reply worker.
                let preserves_selected_reply_read = self
                    .teams
                    .has_selected_remote_channel_reply_for(&team, &channel);
                // Clear any just-superseded feed before the controller starts
                // its worker, even if a future feature event reuses the same
                // selection action shape.
                self.teams.clear_remote_channel_messages();
                self.pending_remote_channel_messages = Some((team, channel));
                // Root-post refresh uses this same action shape. Only a
                // feature-owned selection transition can clear file metadata,
                // so do not cancel the independent channel-files read merely
                // because the user refreshed posts for the same channel.
                if self.teams.take_remote_channel_files_clear_requested() {
                    self.pending_remote_channel_files = None;
                    self.cancel_remote_channel_files_requested = true;
                }
                // A same-channel post refresh leaves the selected root and
                // any controller-bound reply reaction gate intact. Cancel a
                // reply read when the feature reports that its route was
                // abandoned, or when no selected root survived this action.
                if self.teams.take_remote_channel_replies_clear_requested()
                    || !preserves_selected_reply_read
                {
                    self.pending_remote_channel_replies = None;
                    self.cancel_remote_channel_replies_requested = true;
                }
            }
            teams::TeamsAction::RemoteChannelFilesSelected { team, channel } => {
                // The feature has already selected the remote-only Files
                // surface and cleared its old metadata if it changed target.
                // Consume that one-shot signal now so a replacement read can
                // follow the targeted cancellation during the next flush.
                if self.teams.take_remote_channel_files_clear_requested() {
                    self.cancel_remote_channel_files_requested = true;
                }
                self.pending_remote_channel_files = Some((team, channel));
            }
            teams::TeamsAction::RemoteChannelPostSelected {
                team,
                channel,
                post,
            } => {
                // A feature-owned selection change has already cleared its
                // old reply state. Drain that one-shot flag now so it cancels
                // the obsolete read without erasing this replacement request
                // during the app's later transport flush.
                if self.teams.take_remote_channel_replies_clear_requested() {
                    self.cancel_remote_channel_replies_requested = true;
                }
                self.pending_remote_channel_replies = Some((team, channel, post));
            }
        }
    }

    fn run_activity_action(&mut self, action: ActivityAction, toast: &mut ToastState) {
        match action {
            ActivityAction::OpenChannel { channel } => {
                self.open_team_channel(&channel);
                toast.show(format!("Opened {channel}"));
            }
            ActivityAction::OpenTeamPost { channel, post_key } => {
                self.select_local_team_channel(&channel);
                let opened = self.teams.open_selected_post_thread(&post_key);
                self.request_navigation(Section::Teams);
                toast.show(if opened {
                    "Opened the source channel post".into()
                } else {
                    format!("Opened {channel}")
                });
            }
        }
    }

    pub(crate) fn open_team_channel(&mut self, channel: &str) {
        self.select_local_team_channel(channel);
        self.request_navigation(Section::Teams);
    }

    /// Selects local catalog state without allowing an old remote root-post
    /// reply read to survive behind it. Direct Teams UI selection produces
    /// the same feature signal; this helper also covers navigation and search
    /// entry points that deliberately bypass rendered sidebar controls.
    fn select_local_team_channel(&mut self, channel: &str) {
        self.teams.select_team_channel(channel);
        self.discard_pending_remote_channel_files();
        self.discard_pending_remote_channel_replies();
    }

    /// Drops a queued channel-files read and consumes any matching feature
    /// signal. The provider-specific worker cancellation remains app-owned.
    fn discard_pending_remote_channel_files(&mut self) {
        self.pending_remote_channel_files = None;
        self.cancel_remote_channel_files_requested = true;
        let _ = self.teams.take_remote_channel_files_clear_requested();
    }

    /// Drops a queued reply request and consumes any matching feature signal.
    /// The transport cancellation is intentionally deferred to the app so the
    /// coordinator remains provider-agnostic.
    fn discard_pending_remote_channel_replies(&mut self) {
        self.pending_remote_channel_replies = None;
        self.cancel_remote_channel_replies_requested = true;
        let _ = self.teams.take_remote_channel_replies_clear_requested();
    }

    pub(crate) fn request_navigation(&mut self, section: Section) {
        self.navigation_request = Some(section);
    }

    pub(crate) fn take_navigation_request(&mut self) -> Option<Section> {
        self.navigation_request.take()
    }

    /// Drains accepted feature intents so the app-owned transport can stage
    /// them without exposing a provider client to any feature module.
    pub(crate) fn take_transport_commands(&mut self) -> Vec<WorkspaceCommand> {
        let mut commands = std::mem::take(&mut self.pending_transport_commands);
        commands.extend(
            self.threads
                .take_changes()
                .into_iter()
                .filter_map(workspace_command_for_thread_change),
        );
        commands.extend(
            self.calendar
                .take_actions()
                .into_iter()
                .map(|action| match action {
                    CalendarAction::CreateEvent {
                        title,
                        starts_at,
                        ends_at,
                        online_meeting,
                    } => WorkspaceCommand::CreateCalendarEvent {
                        // The local preview has one primary calendar. This is
                        // explicitly a local resource key, not a Graph
                        // calendar ID. A remote projection must provide a
                        // provider-issued target before it enables creation.
                        calendar: ProviderResourceRef::local_preview("local-primary"),
                        title,
                        starts_at,
                        ends_at,
                        online_meeting,
                    },
                }),
        );
        commands
    }

    /// Drains accepted remote root-post requests without exposing a provider
    /// client to the Teams feature. The app must enqueue exactly one command
    /// per record and bind the returned intent before processing outcomes.
    pub(crate) fn take_remote_channel_post_requests(
        &mut self,
    ) -> Vec<(
        RemoteChannelPostSubmissionId,
        ProviderResourceRef,
        ProviderResourceRef,
        String,
    )> {
        std::mem::take(&mut self.pending_remote_channel_posts)
            .into_iter()
            .map(|request| {
                (
                    request.submission,
                    request.team,
                    request.channel,
                    request.body,
                )
            })
            .collect()
    }

    /// Correlates one feature-local remote channel-post submission with the
    /// transport's opaque intent ID. No provider message ID or response DTO
    /// crosses this seam.
    pub(crate) fn bind_remote_channel_post_submission(
        &mut self,
        submission: RemoteChannelPostSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams
            .bind_remote_channel_post_submission(submission, intent_id)
    }

    pub(crate) fn remote_channel_post_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.teams.remote_channel_post_delivered(intent_id)
    }

    pub(crate) fn remote_channel_post_retry_scheduled(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_post_retry_scheduled(intent_id)
    }

    pub(crate) fn remote_channel_post_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.teams.remote_channel_post_failed(intent_id)
    }

    pub(crate) fn remote_channel_post_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_post_delivery_uncertain(intent_id)
    }

    /// Drains accepted remote root-reply requests without exposing a provider
    /// client to the Teams feature. The app must enqueue exactly one
    /// `ReplyToChannelPost` command per record and bind its intent before it
    /// handles dispatch outcomes.
    pub(crate) fn take_remote_channel_reply_requests(
        &mut self,
    ) -> Vec<(
        RemoteChannelReplySubmissionId,
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
        String,
    )> {
        std::mem::take(&mut self.pending_remote_channel_reply_writes)
            .into_iter()
            .map(|request| {
                (
                    request.submission,
                    request.team,
                    request.channel,
                    request.post,
                    request.body,
                )
            })
            .collect()
    }

    /// Correlates one feature-local remote root-reply submission with the
    /// controller-owned intent. Provider response data never crosses this
    /// seam.
    pub(crate) fn bind_remote_channel_reply_submission(
        &mut self,
        submission: RemoteChannelReplySubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams
            .bind_remote_channel_reply_submission(submission, intent_id)
    }

    pub(crate) fn remote_channel_reply_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.teams.remote_channel_reply_delivered(intent_id)
    }

    pub(crate) fn remote_channel_reply_retry_scheduled(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_reply_retry_scheduled(intent_id)
    }

    pub(crate) fn remote_channel_reply_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.teams.remote_channel_reply_failed(intent_id)
    }

    pub(crate) fn remote_channel_reply_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams
            .remote_channel_reply_delivery_uncertain(intent_id)
    }

    /// Drains exact root-post reaction requests. Unlike a local reaction,
    /// every provider route segment is retained until the app queues the
    /// Microsoft 365 command and binds the resulting controller intent.
    pub(crate) fn take_remote_channel_post_reaction_requests(
        &mut self,
    ) -> Vec<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
        ReactionKind,
        bool,
    )> {
        std::mem::take(&mut self.pending_remote_channel_post_reactions)
            .into_iter()
            // A UI click can become stale between `run_teams_action` and this
            // narrow app handoff: for example, a directory refresh can omit
            // its channel. The Teams feature drops that *unbound* gate during
            // cleanup, so never turn the orphaned coordinator entry into a
            // Graph mutation. A bound tombstone also deliberately does not
            // match: it already belongs to the transport FIFO.
            .filter(|request| {
                self.teams.has_unbound_remote_channel_post_reaction(
                    &request.team,
                    &request.channel,
                    &request.post,
                    request.reaction,
                    request.selected,
                )
            })
            .map(|request| {
                (
                    request.team,
                    request.channel,
                    request.post,
                    request.reaction,
                    request.selected,
                )
            })
            .collect()
    }

    /// Binds a feature-owned reaction gate to its controller intent. This
    /// comparison is intentionally route-complete so a same-ID post in a
    /// different channel cannot be unlocked by a late terminal outcome.
    pub(crate) fn bind_remote_channel_post_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.teams
            .bind_remote_channel_post_reaction(intent_id, team, channel, post, reaction)
    }

    /// Releases only a definitively rejected remote reaction. Confirmed or
    /// ambiguous requests remain locked until their exact bounded provider
    /// feed has replaced the old reaction state.
    pub(crate) fn remote_channel_post_reaction_failed(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_post_reaction_failed(intent_id)
    }

    /// Whether this exact root-post reaction intent is still owned by the feature.
    /// Delivery, throttled retry and uncertain outcomes all keep the same gate
    /// until a matching provider feed arrives, so one predicate serves them all.
    pub(crate) fn remote_channel_post_reaction_owned(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_post_reaction_owned(intent_id)
    }

    /// Drains exact selected-root reply reaction requests. This remains a
    /// route-complete app handoff rather than a local generic reaction or a
    /// selected-root `ChannelReplies` read request.
    pub(crate) fn take_remote_channel_reply_reaction_requests(
        &mut self,
    ) -> Vec<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
        ReactionKind,
        bool,
    )> {
        std::mem::take(&mut self.pending_remote_channel_reply_reactions)
            .into_iter()
            // Apply the same fail-closed rule to the full reply route. Do not
            // let a stale coordinator handoff outlive the feature-owned
            // unbound gate, and do not mistake a bound delivery-recovery
            // tombstone for a second write.
            .filter(|request| {
                self.teams.has_unbound_remote_channel_reply_reaction(
                    &request.team,
                    &request.channel,
                    &request.post,
                    &request.reply,
                    request.reaction,
                    request.selected,
                )
            })
            .map(|request| {
                (
                    request.team,
                    request.channel,
                    request.post,
                    request.reply,
                    request.reaction,
                    request.selected,
                )
            })
            .collect()
    }

    /// Binds a feature-owned reply-reaction gate to one controller intent.
    /// Every route segment participates so same-ID replies in another root or
    /// channel cannot be unlocked by a late terminal outcome.
    pub(crate) fn bind_remote_channel_reply_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.teams
            .bind_remote_channel_reply_reaction(intent_id, team, channel, post, reply, reaction)
    }

    /// Releases only a definitively rejected reply reaction. Confirmed and
    /// ambiguous actions stay gated until a matching bounded reply projection
    /// resolves them.
    pub(crate) fn remote_channel_reply_reaction_failed(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_reply_reaction_failed(intent_id)
    }

    /// Whether this exact reply reaction intent is still owned by the feature
    /// (delivered, throttled retry and uncertain outcomes share one predicate).
    pub(crate) fn remote_channel_reply_reaction_owned(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.teams.remote_channel_reply_reaction_owned(intent_id)
    }

    /// Starts a keep-rows refresh of the selected remote channel's posts for
    /// an explicit user event. Returns the opaque targets to read.
    pub(crate) fn begin_remote_channel_messages_refresh(
        &mut self,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        self.teams.begin_remote_channel_messages_refresh()
    }

    /// Starts a keep-rows refresh of the selected remote root post's replies.
    pub(crate) fn begin_remote_channel_replies_refresh(
        &mut self,
    ) -> Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )> {
        self.teams.begin_remote_channel_replies_refresh()
    }

    /// Drains one selection-scoped remote channel read. Keeping this separate
    /// from `WorkspaceCommand` preserves the difference between a read
    /// projection and a user-visible provider mutation.
    pub(crate) fn take_remote_channel_messages_request(
        &mut self,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        self.pending_remote_channel_messages.take()
    }

    /// Drains one selected-channel backing-folder metadata read. This is a
    /// bounded provider projection, not a `WorkspaceCommand` or a global
    /// OneDrive request.
    pub(crate) fn take_remote_channel_files_request(
        &mut self,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        self.pending_remote_channel_files.take()
    }

    /// Drains one newest selected-root-post read. This remains distinct from
    /// `WorkspaceCommand`, preserving the difference between a bounded
    /// projection and a remote reply mutation.
    pub(crate) fn take_remote_channel_replies_request(
        &mut self,
    ) -> Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )> {
        self.pending_remote_channel_replies.take()
    }

    /// Drains the feature's one-shot reply-clear signal. A direct local UI
    /// channel selection, a remote directory replacement, or post removal
    /// has no matching action here, so it must also discard the stale queued
    /// read before the app asks the controller to cancel that reply poll.
    pub(crate) fn take_remote_channel_replies_cancellation_requested(&mut self) -> bool {
        if self.teams.take_remote_channel_replies_clear_requested() {
            self.pending_remote_channel_replies = None;
            self.cancel_remote_channel_replies_requested = true;
        }
        std::mem::take(&mut self.cancel_remote_channel_replies_requested)
    }

    /// Drains the feature's one-shot selected-channel file clear signal. A
    /// channel/local/account transition must purge only matching file reads,
    /// never root-post/reply polling or provider command delivery.
    pub(crate) fn take_remote_channel_files_cancellation_requested(&mut self) -> bool {
        if self.teams.take_remote_channel_files_clear_requested() {
            self.pending_remote_channel_files = None;
            self.cancel_remote_channel_files_requested = true;
        }
        std::mem::take(&mut self.cancel_remote_channel_files_requested)
    }

    /// Exercises the same selection-to-transport handoff as the Teams UI
    /// without making provider-backed selection controls public in production.
    #[cfg(test)]
    pub(crate) fn select_remote_channel_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        if !self
            .teams
            .select_remote_channel_for_test(team.clone(), channel.clone())
        {
            return false;
        }
        let mut toast = ToastState::default();
        self.run_teams_action(
            teams::TeamsAction::RemoteChannelSelected { team, channel },
            &mut toast,
        );
        true
    }

    /// Opens the remote channel a Microsoft 365 search hit points at, through
    /// the same selection action as a sidebar click so its posts load. `false`
    /// when the Teams directory does not contain it.
    pub(crate) fn open_remote_channel(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        toast: &mut ToastState,
    ) -> bool {
        let already_selected = self.teams.remote_channel_is_selected(&team, &channel);
        if !self
            .teams
            .open_remote_channel(team.clone(), channel.clone())
        {
            return false;
        }
        if !already_selected {
            self.run_teams_action(
                teams::TeamsAction::RemoteChannelSelected { team, channel },
                toast,
            );
        }
        true
    }

    /// Exercises selected-root-post reply loading through the same action
    /// handoff as the rendered remote channel page. It deliberately keeps the
    /// provider targets opaque even in app lifecycle tests.
    #[cfg(test)]
    pub(crate) fn select_remote_channel_post_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> bool {
        if !self.teams.select_remote_channel_post_for_test(
            team.clone(),
            channel.clone(),
            post.clone(),
        ) {
            return false;
        }
        let mut toast = ToastState::default();
        self.run_teams_action(
            teams::TeamsAction::RemoteChannelPostSelected {
                team,
                channel,
                post,
            },
            &mut toast,
        );
        true
    }

    /// Exercises selected-channel file loading through the same read-only
    /// action handoff as the remote Teams Files surface. The backing drive and
    /// folder IDs remain hidden even in lifecycle tests.
    #[cfg(test)]
    pub(crate) fn select_remote_channel_files_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        if !self
            .teams
            .select_remote_channel_files_for_test(team.clone(), channel.clone())
        {
            return false;
        }
        let mut toast = ToastState::default();
        self.run_teams_action(
            teams::TeamsAction::RemoteChannelFilesSelected { team, channel },
            &mut toast,
        );
        true
    }

    #[cfg(test)]
    pub(crate) fn teams(&self) -> &TeamsState {
        &self.teams
    }

    /// Opens the new-meeting form in the calendar.
    pub(crate) fn open_new_meeting(&mut self) {
        self.calendar.begin_new_meeting();
    }

    #[cfg(test)]
    pub(crate) fn teams_mut(&mut self) -> &mut TeamsState {
        &mut self.teams
    }

    #[cfg(test)]
    pub(crate) fn calendar(&self) -> &CalendarState {
        &self.calendar
    }

    #[cfg(test)]
    pub(crate) fn threads_mut(&mut self) -> &mut ThreadStore {
        &mut self.threads
    }

    #[cfg(test)]
    pub(crate) fn files_mut(&mut self) -> &mut FilesState {
        &mut self.files
    }
}

fn workspace_command_for_thread_change(change: ThreadChange) -> Option<WorkspaceCommand> {
    match change {
        ThreadChange::ReplyAdded { thread, reply } => {
            if !teams::is_transportable_thread(&thread) {
                return None;
            }
            let team = team_for_channel(thread.channel())?;
            Some(WorkspaceCommand::ReplyToChannelPost {
                team: ProviderResourceRef::local_preview(team.name),
                channel: ProviderResourceRef::local_preview(thread.channel()),
                post: ProviderResourceRef::local_preview(thread.post_key()),
                body: reply.body().into(),
            })
        }
        // Locally authored reply IDs do not have provider-issued message IDs.
        // Preserve these mutations in the canonical snapshot, but never
        // speculate about remote update/delete endpoints until an adapter can
        // reconcile them idempotently.
        ThreadChange::ReplyEdited { .. } | ThreadChange::ReplyDeleted { .. } => None,
        ThreadChange::ReactionChanged {
            thread,
            target,
            reaction,
            selected,
        } => {
            if !teams::is_transportable_thread_target(&thread, &target) {
                return None;
            }
            let message = match target {
                ThreadTarget::Post => ProviderResourceRef::local_preview(thread.post_key()),
                ThreadTarget::SourceReply { reply_key } => ProviderResourceRef::local_preview(
                    format!("{}:reply:{reply_key}", thread.post_key()),
                ),
                // `is_transportable_thread_target` already excludes local
                // replies; retain this guard if the Teams catalog evolves.
                ThreadTarget::LocalReply { .. } => return None,
            };
            Some(WorkspaceCommand::SetMessageReaction {
                message,
                reaction,
                selected,
            })
        }
    }
}

#[cfg(test)]
mod tests;
