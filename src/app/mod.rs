pub(crate) mod app_prefs;
pub(crate) mod atomic_file;
pub(crate) mod auth;
mod browser;
mod local_stores;
mod navigation;
mod os_notify;
pub(crate) mod persistence;
pub(crate) mod presence;
mod rail;
pub(crate) mod remote_auth;
mod remote_projection;
mod remote_refresh;
#[cfg(feature = "screenshot")]
pub(crate) mod screenshot;
pub(crate) mod session;
#[cfg(all(test, feature = "ui-snapshots"))]
mod snapshot_tests;
pub(crate) mod storage;
pub(crate) mod workspace_coordinator;
pub(crate) mod workspace_transport;

use crate::app::auth::AuthState;
use crate::app::browser::BrowserOpenError;
use crate::app::local_stores::{LocalStores, ThreadMigration};
use crate::app::navigation::Navigation;
use crate::app::rail::chrome_icon_button;
use crate::app::remote_auth::RemoteAuthorizationController;
use crate::app::remote_projection::{
    remote_calendar_directory_from_projection, remote_channel_files_directory_from_projection,
    remote_channel_messages_directory_from_projection,
    remote_channel_replies_directory_from_projection, remote_file_preview_from_projection,
    remote_files_directory_from_projection, remote_teams_directory_from_projection,
};
use crate::app::remote_refresh::{
    RefreshTarget, RefreshTrigger, RemoteRefreshScheduler, window_focused,
};
use crate::app::workspace_coordinator::WorkspaceState;
use crate::app::workspace_transport::{CommandDispatch, WorkspaceTransportController};

/// Safety-net repaint interval while a remote read or command is in flight.
/// Workers request a repaint themselves as soon as they hand a result back.
const REMOTE_WORK_REPAINT_FALLBACK: std::time::Duration = std::time::Duration::from_millis(500);
use crate::data::workspace::workspace;
use crate::features::chat::{ChatAction, ChatState};
use crate::features::palette::{PaletteAction, PaletteContext, PaletteState};
use crate::features::search::{SearchAction, SearchState};
use crate::features::settings::{
    Microsoft365View, SettingsAction, SettingsSection, SettingsState, SettingsView,
};
use crate::shared::model::Section;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize};
use crate::ui::modal;
use crate::ui::shortcuts::take_escape;
use crate::ui::theme;
use crate::ui::toast::{ToastState, UndoKind};
use eframe::egui::{self, Align, Layout, Popup, RichText, Stroke};

const RAIL_DESTINATIONS: [(Section, &str, egui::Key); 6] = [
    (Section::Activity, "Activity", egui::Key::Num1),
    (Section::Chat, "Chat", egui::Key::Num2),
    (Section::Teams, "Teams", egui::Key::Num3),
    (Section::Calendar, "Calendar", egui::Key::Num4),
    (Section::Calls, "Calls", egui::Key::Num5),
    (Section::Files, "Files", egui::Key::Num6),
];

const REFERENCE_WINDOW_SIZE: [f32; 2] = [1410.0, 860.0];
// The supplied desktop captures use a compact app rail and contextual list so
// the conversation canvas remains the visual focus at the reference viewport.
const TOPBAR_HEIGHT: f32 = 48.0;
const RAIL_WIDTH: f32 = 50.0;
const RAIL_ITEM_SIZE: f32 = 44.0;
const SIDEBAR_WIDTH: f32 = 288.0;
const TOPBAR_ACCOUNT_WIDTH: f32 = 280.0;
const TOPBAR_ITEM_SPACING: f32 = 8.0;
const TOPBAR_SEARCH_MAX_WIDTH: f32 = 640.0;
const TOPBAR_ORGANIZATION_WIDTH: f32 = 140.0;
const TOPBAR_SEARCH_HEIGHT: f32 = 32.0;

/// Opens only direct, already queued remote browser requests. Keeping the
/// launch function injectable makes the browser boundary testable without
/// starting a host process; production passes `browser::open_https_url` so each
/// external provider destination is revalidated immediately before launch.
fn launch_remote_browser_handoff_links<T: AsRef<str>>(
    destinations: Vec<T>,
    mut launch: impl FnMut(&str) -> Result<(), BrowserOpenError>,
) -> Vec<Result<(), BrowserOpenError>> {
    destinations
        .into_iter()
        .map(|destination| launch(destination.as_ref()))
        .collect()
}

fn remote_meeting_join_toast(result: Result<(), BrowserOpenError>) -> String {
    match result {
        Ok(()) => "Opening the Microsoft 365 meeting in your system browser…".into(),
        Err(error) => format!("The Microsoft 365 meeting could not be opened ({error})."),
    }
}

fn remote_file_open_toast(result: Result<(), BrowserOpenError>) -> String {
    match result {
        Ok(()) => "Opening the Microsoft 365 item in your system browser…".into(),
        Err(error) => format!("The Microsoft 365 item could not be opened ({error})."),
    }
}

fn remote_chat_link_toast(result: Result<(), BrowserOpenError>) -> Result<String, String> {
    match result {
        Ok(()) => Ok("Opening the link in your system browser…".into()),
        Err(error) => Err(format!("The link could not be opened ({error}).")),
    }
}

/// A chat-attachment upload command in flight. A cancel command is tracked
/// too, so its outcome is consumed silently.
struct UploadIntent {
    intent: crate::integrations::WorkspaceIntentId,
    chat: crate::integrations::ProviderResourceRef,
    cancel: bool,
    /// The command shares an uploaded file with the chat's members: its
    /// outcome is a warning at most, and never reaches the composer.
    share: bool,
}

fn topbar_search_width(available_width: f32) -> f32 {
    (available_width - TOPBAR_ACCOUNT_WIDTH - TOPBAR_ITEM_SPACING)
        .clamp(300.0, TOPBAR_SEARCH_MAX_WIDTH)
}

/// The rounded white global search field with a leading search icon.
fn draw_search_box(
    ui: &mut egui::Ui,
    id: egui::Id,
    query: &mut String,
    width: f32,
) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, TOPBAR_SEARCH_HEIGHT),
        egui::Sense::hover(),
    );
    let focused = ui.memory(|memory| memory.has_focus(id));
    let stroke_color = if focused {
        theme::brand_fg()
    } else {
        theme::stroke_subtle()
    };
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(theme::radius::MD),
        theme::surface(),
        Stroke::new(1.0_f32, stroke_color),
        egui::StrokeKind::Inside,
    );
    let icon_side = IconSize::S16.px();
    Icon::Search.image(IconSize::S16).paint_at(
        ui,
        egui::Rect::from_center_size(
            egui::pos2(
                rect.left() + theme::space::LG + icon_side / 2.0,
                rect.center().y,
            ),
            egui::vec2(icon_side, icon_side),
        ),
    );
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + theme::space::LG * 2.0 + icon_side, rect.top()),
        egui::pos2(rect.right() - theme::space::MD, rect.bottom()),
    );
    ui.put(
        text_rect,
        egui::TextEdit::singleline(query)
            .id(id)
            .frame(false)
            .desired_width(text_rect.width())
            .vertical_align(Align::Center)
            .hint_text(
                RichText::new(format!(
                    "Search ({})",
                    crate::ui::shortcuts::primary_chord("E")
                ))
                .font(theme::regular(theme::font_size::BODY))
                .color(theme::fg3()),
            ),
    )
}

fn rail_shortcut_section(key: egui::Key) -> Option<Section> {
    RAIL_DESTINATIONS
        .iter()
        .find_map(|(section, _, shortcut)| (*shortcut == key).then_some(*section))
}

/// Writes only what the controller can know about an ambiguous channel POST.
/// A live adapter can attempt one bounded presentation refresh, whereas a
/// lost adapter has no safe way to refresh or resend the post.
const fn mutation_noun(label: crate::features::chat::RemoteMutationLabel) -> &'static str {
    match label {
        crate::features::chat::RemoteMutationLabel::Edit => "edit",
        crate::features::chat::RemoteMutationLabel::Delete => "deletion",
    }
}

fn remote_mutation_rejected_toast(
    label: crate::features::chat::RemoteMutationLabel,
    forbidden: bool,
    error: &crate::integrations::TransportError,
) -> String {
    use crate::features::chat::RemoteMutationLabel;
    if forbidden {
        return match label {
            RemoteMutationLabel::Edit => {
                "Microsoft 365 did not allow editing messages. Grant the optional Chat.ReadWrite permission and sign in again."
                    .into()
            }
            RemoteMutationLabel::Delete => {
                "Microsoft 365 did not allow deleting messages. Grant the optional Chat.ReadWrite permission and sign in again."
                    .into()
            }
        };
    }
    if matches!(error, crate::integrations::TransportError::Cancelled) {
        return format!(
            "Microsoft 365 no longer included that chat, so the {} was not sent.",
            mutation_noun(label)
        );
    }
    format!(
        "Microsoft 365 did not apply the {}; the message is unchanged ({error}).",
        mutation_noun(label)
    )
}

fn remote_mutation_resolution_toast(
    resolution: crate::features::chat::RemoteMutationResolution,
) -> &'static str {
    use crate::features::chat::RemoteMutationLabel;
    match (resolution.label, resolution.confirmed) {
        (RemoteMutationLabel::Edit, true) => "Microsoft 365 confirmed your edit.",
        (RemoteMutationLabel::Edit, false) => {
            "Microsoft 365 did not show the edit; your text is ready to review and save again."
        }
        (RemoteMutationLabel::Delete, true) => "Microsoft 365 confirmed the message was deleted.",
        (RemoteMutationLabel::Delete, false) => {
            "Could not confirm the deletion; the message is unchanged as far as Microsoft 365 shows."
        }
    }
}

fn remote_channel_post_delivery_uncertain_toast(
    microsoft365_active: bool,
    error: &crate::integrations::TransportError,
) -> String {
    if microsoft365_active {
        format!(
            "Microsoft 365 could not confirm the channel post. A bounded channel refresh is attempted, but it cannot prove whether the post was delivered; review the retained draft before any retry ({error})."
        )
    } else {
        format!(
            "Microsoft 365 could not confirm the channel post. The connection is unavailable, so no channel refresh is possible; reconnect and inspect the channel before deciding whether to post again ({error})."
        )
    }
}

/// Writes only what the controller can know about an ambiguous reply POST.
/// A live adapter can refresh precisely that root's replies, whereas a lost
/// adapter cannot safely refresh or resend the reply.
fn remote_channel_reply_delivery_uncertain_toast(
    microsoft365_active: bool,
    error: &crate::integrations::TransportError,
) -> String {
    if microsoft365_active {
        format!(
            "Microsoft 365 could not confirm the channel reply. A bounded reply refresh is attempted, but it cannot prove whether the reply was delivered; review the retained draft before any retry ({error})."
        )
    } else {
        format!(
            "Microsoft 365 could not confirm the channel reply. The connection is unavailable, so no reply refresh is possible; reconnect and inspect the reply thread before deciding whether to reply again ({error})."
        )
    }
}

struct TeamsApp {
    nav: Navigation,
    chat: ChatState,
    search: SearchState,
    workspace: WorkspaceState,
    toast: ToastState,
    auth: AuthState,
    remote_auth: RemoteAuthorizationController,
    shortcuts_open: bool,
    /// Transient: the Ctrl/Cmd+K command palette. Never persisted.
    palette: PaletteState,
    /// Transient: the Settings dialog. Never persisted.
    settings: SettingsState,
    /// App-wide per-profile choices (today the colour theme).
    app_prefs: app_prefs::AppPrefs,
    app_prefs_dirty: bool,
    /// Transient: the contextual list sidebar is hidden. Never persisted.
    sidebar_collapsed: bool,
    local_persistence_enabled: bool,
    stores: LocalStores,
    transport: WorkspaceTransportController,
    pending_local_storage_notice: Option<String>,
    /// Set once a close request was held back because a store could not be
    /// saved, so a second close request quits anyway.
    close_blocked_by_save_failure: bool,
    thread_migration: ThreadMigration,
    /// Event-driven refresh policy; there is deliberately no timer.
    refresh: RemoteRefreshScheduler,
    /// Controller intents of in-flight read receipts. Their outcomes are
    /// silent (a receipt is best-effort and never reconciled) except that a
    /// `403` turns receipts off for the session.
    read_receipt_intents: Vec<crate::integrations::WorkspaceIntentId>,
    /// Upload commands for chat attachments in flight. Their outcome is
    /// consumed here even if the composer that asked has since gone.
    upload_intents: Vec<UploadIntent>,
    /// A session upload took a fragment: another frame must drain the next
    /// fragment's command.
    upload_progressed: bool,
    /// System notifications: disabled unless the `os-notifications` feature
    /// is built and the process has a truthful identity.
    os_notifier: os_notify::OsNotifier,
}

impl Default for TeamsApp {
    fn default() -> Self {
        Self::from_parts(AuthState::default(), Section::Chat, false)
    }
}

impl TeamsApp {
    /// The single place a `TeamsApp` is assembled. Every entry point goes
    /// through it so a section's initial state is identical everywhere.
    fn from_parts(auth: AuthState, section: Section, local_persistence_enabled: bool) -> Self {
        Self {
            nav: Navigation::new(section),
            chat: ChatState::default(),
            search: SearchState::default(),
            workspace: WorkspaceState::default(),
            toast: ToastState::default(),
            auth,
            remote_auth: RemoteAuthorizationController::default(),
            shortcuts_open: false,
            palette: PaletteState::default(),
            settings: SettingsState::default(),
            app_prefs: app_prefs::AppPrefs::default(),
            app_prefs_dirty: false,
            sidebar_collapsed: false,
            local_persistence_enabled,
            stores: LocalStores::default(),
            transport: WorkspaceTransportController::default(),
            pending_local_storage_notice: None,
            close_blocked_by_save_failure: false,
            thread_migration: ThreadMigration::default(),
            refresh: RemoteRefreshScheduler::default(),
            read_receipt_intents: Vec::new(),
            upload_intents: Vec::new(),
            upload_progressed: false,
            os_notifier: os_notify::OsNotifier::disabled(),
        }
    }

    /// Creation-time setup shared by the real window and off-screen renders.
    fn create(cc: &eframe::CreationContext<'_>, mut app: Self) -> Self {
        let ctx = cc.egui_ctx.clone();
        app.transport
            .set_repaint_waker(std::sync::Arc::new(move || ctx.request_repaint()));
        egui_extras::install_image_loaders(&cc.egui_ctx);
        crate::ui::theme::apply(&cc.egui_ctx);
        app.os_notifier = os_notify::OsNotifier::start();
        app.chat
            .set_os_notifications_available(app.os_notifier.is_available());
        app
    }

    /// Isolated, deterministic local-preview app: no stored session, no
    /// profile persistence, no network.
    #[cfg(any(test, feature = "screenshot"))]
    fn local_preview(section: Section) -> Self {
        Self::from_parts(AuthState::local_preview(), section, false)
    }
}

impl eframe::App for TeamsApp {
    /// Last chance to persist: runs once after the final frame, on every
    /// shutdown path including those that never raise a close request. A
    /// failure can only be reported on stderr because no UI is left.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let failures = self.flush_all_local_stores_now(&egui::Context::default());
        for failure in failures {
            eprintln!("{failure}");
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First, so every colour this frame reads the chosen scheme.
        theme::activate(ctx, self.app_prefs.theme);
        self.handle_close_request(ctx);
        if self.auth.render_gate(ctx) {
            self.workspace
                .forget_discarded_remote_file_preview_images(ctx);
            return;
        }
        self.ensure_local_presence_store();
        self.ensure_local_chat_store();
        self.ensure_local_chat_prefs_store();
        self.ensure_local_app_prefs_store();
        self.ensure_local_calendar_store();
        self.ensure_local_activity_store();
        self.ensure_local_teams_store();
        self.ensure_local_threads_store();
        self.ensure_local_calls_store();
        self.ensure_local_files_store();
        self.collect_remote_authorization(ctx);
        self.observe_window_focus(ctx);
        self.synchronize_workspace_transport(ctx);
        if let Some(notice) = self.auth.take_authenticated_notice() {
            self.toast.show(notice);
        }
        if let Some(notice) = self.pending_local_storage_notice.take() {
            self.toast.show(notice);
        }

        self.handle_palette_shortcut(ctx);
        self.handle_settings_shortcut(ctx);
        self.handle_rail_shortcut(ctx);
        let global_search_enabled = self.global_search_enabled();
        let search_input_focused = self.draw_topbar(ctx, global_search_enabled);
        let remote_available = self.transport.is_microsoft365_active();
        if let Some(action) = self.search.draw_results(
            ctx,
            &self.chat,
            &self.workspace,
            remote_available,
            search_input_focused,
        ) {
            self.run_search_action(action);
        }
        self.drive_remote_message_search();
        if let Some(notice) = self.chat.take_search_focus_notice() {
            self.toast.show(notice);
        }
        let palette_context = PaletteContext {
            refresh_available: self.transport.is_microsoft365_active(),
            sidebar_collapsed: self.sidebar_collapsed,
        };
        if let Some(action) = self.palette.draw(ctx, palette_context) {
            self.run_palette_action(action);
        }
        self.draw_rail(ctx);
        self.draw_local_storage_warnings(ctx);
        self.auth.render_profile_menu(ctx);
        if !self.auth.is_authenticated() {
            // Sign-out replaces the workspace immediately below. Evict any
            // egui loader/texture state before that session-only Files state
            // is dropped, even though the Files surface is no longer drawn.
            self.workspace
                .forget_discarded_remote_file_preview_images(ctx);
            self.remote_auth.cancel();
            self.reset_local_session(ctx);
            return;
        }
        self.draw_sidebar(ctx);
        self.draw_surface(ctx);
        // Files can dismiss an image through navigation, a projection clear,
        // escape, or its close control. Drain its cache keys at the app/UI
        // boundary rather than only while its page is visible.
        self.workspace
            .forget_discarded_remote_file_preview_images(ctx);
        self.flush_workspace_transport(ctx);
        self.workspace.draw_transient_overlays(ctx, &mut self.toast);
        self.draw_shortcuts_dialog(ctx);
        self.draw_settings_dialog(ctx);
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
        self.toast.draw(ctx);
        self.apply_requested_undo();
    }
}

impl TeamsApp {
    /// The production factory opts into on-device local feature persistence.
    /// `Default` intentionally leaves it disabled so tests cannot write into
    /// a user's profile cache.
    fn with_local_persistence() -> Self {
        Self::from_parts(AuthState::restored(), Section::Chat, true)
    }

    fn synchronize_workspace_transport(&mut self, ctx: &egui::Context) {
        if let Err(error) = self
            .transport
            .synchronize_profile(self.auth.local_profile())
        {
            self.toast
                .show_error(format!("Workspace transport is unavailable ({error})."));
        }
        if self.transport.take_remote_projection_clear_requested() {
            // A new session must not inherit the old one's refresh spacing.
            self.refresh.reset();
            let preserving_delivery_recovery = self
                .transport
                .take_remote_projection_clear_for_transport_loss_requested();
            if preserving_delivery_recovery {
                self.chat
                    .clear_remote_projection_preserving_delivery_recovery();
                self.workspace
                    .clear_remote_teams_directory_preserving_delivery_recovery();
            } else {
                self.chat.clear_remote_projection();
                self.workspace.clear_remote_teams_directory();
            }
            self.workspace.clear_remote_calendar_directory();
            self.workspace.clear_remote_files_directory();
            self.workspace
                .forget_discarded_remote_file_preview_images(ctx);
        }
        for delta in self.transport.take_deltas() {
            if let Some(identity) = delta.identity.as_ref() {
                // Keys the per-profile mute digests; session only.
                self.chat.set_remote_account(&identity.account_id);
            }
            if let Some(capabilities) = delta.remote_capabilities {
                // Session-only: what this sign-in may do with its own chat
                // messages. It gates affordances and is never persisted.
                self.chat.apply_remote_capabilities(capabilities);
            }
            if let Some(projection) = delta.remote_chats {
                // The feature owns the presentation projection; the app only
                // hands across a normalized integration result. The snapshot
                // remains ephemeral and is never included in local storage.
                self.refresh
                    .note_read(RefreshTarget::Directory, std::time::Instant::now());
                let cancel_unstarted = self.chat.apply_remote_projection(projection);
                self.transport
                    .cancel_queued_remote_intents(cancel_unstarted);
            }
            if let Some(projection) = delta.remote_chat_messages {
                // A selected history is target-bound inside the feature. A
                // stale worker result is intentionally ignored there instead
                // of ever becoming a local timeline or a global chat refresh.
                let _ = self.chat.apply_remote_chat_messages_projection(projection);
            }
            if let Some(projection) = delta.remote_pinned_chat_messages {
                // Pinned messages are a separate, target-bound contextual
                // read. The Chat feature rejects a result after its panel or
                // selected conversation changes; it never becomes a local
                // record or a replacement for the main message history.
                let _ = self
                    .chat
                    .apply_remote_chat_pinned_messages_projection(projection);
            }
            if let Some(projection) = delta.remote_message_search {
                // A search page answers the exact query and page awaited by
                // the overlay; anything else (an earlier query) is ignored
                // there. It is never persisted.
                self.search.apply_remote_projection(projection);
            }
            if let Some(projection) = delta.remote_chat_members {
                // A selected People roster is an independent, target-bound
                // contextual read. The Chat feature rejects late or closed
                // panel results; this app boundary never turns it into a
                // local record or broad chat-directory update.
                let _ = self.chat.apply_remote_chat_members_projection(projection);
            }
            if let Some(projection) = delta.remote_teams {
                self.workspace.replace_remote_teams_directory(
                    remote_teams_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_channel_messages {
                let _ = self.workspace.replace_remote_channel_messages(
                    remote_channel_messages_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_channel_files {
                let _ = self.workspace.replace_remote_channel_files(
                    remote_channel_files_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_channel_replies {
                let _ = self.workspace.replace_remote_channel_replies(
                    remote_channel_replies_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_calendar {
                self.workspace.replace_remote_calendar_directory(
                    remote_calendar_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_files {
                self.workspace.replace_remote_files_directory(
                    remote_files_directory_from_projection(projection),
                );
            }
            if let Some(projection) = delta.remote_file_preview {
                // The Files feature accepts this only while the same current
                // OneDrive file remains selected and loading. A late worker
                // result therefore cannot reopen a cleared directory or leak
                // its session-only bytes into any local Files state.
                let _ = self
                    .workspace
                    .apply_remote_file_preview(remote_file_preview_from_projection(projection));
            }
        }
        if let Some(notice) = self.transport.take_notice() {
            self.toast.show(notice);
        }
        self.announce_new_messages();
        if self.transport.is_polling() {
            // The worker wakes the UI when it finishes; this only bounds the
            // wait if a wake-up is ever missed (for example a panicked worker).
            ctx.request_repaint_after(REMOTE_WORK_REPAINT_FALLBACK);
        }
    }

    /// Runs the Undo the user chose on a toast: each local delete is already
    /// persisted, so this re-inserts the record and marks its store dirty.
    fn apply_requested_undo(&mut self) {
        let Some(kind) = self.toast.take_undo() else {
            return;
        };
        let (restored, text) = match kind {
            UndoKind::ChatMessage => {
                if self.chat.undo_delete_local_message() {
                    (true, "Restored the message")
                } else {
                    (false, "Couldn't restore that message.")
                }
            }
            other => match self.workspace.undo_local_delete(other) {
                Some(outcome) => outcome,
                None => return,
            },
        };
        if restored {
            self.toast.show_success(text);
        } else {
            self.toast.show_error(text);
        }
    }

    /// The new-message toast (a count, never a body), after any notice, and
    /// the same text as a system notification while the user is away.
    fn announce_new_messages(&mut self) {
        let Some(text) = self.chat.take_notification_toast() else {
            return;
        };
        // In view, the in-app toast is enough; the system one is for when the
        // window is not focused.
        if !self.refresh.focused() && self.chat.os_notifications_wanted() {
            let _ = self.os_notifier.notify(&text);
        }
        self.toast.show(text);
    }

    /// Feeds this frame's window focus to the refresh scheduler, the chat
    /// read acknowledgement and (through `refresh.focused()`) presence. This
    /// is the single focus definition; nothing else reads the viewport.
    fn observe_window_focus(&mut self, ctx: &egui::Context) {
        let focused = window_focused(ctx);
        self.chat.set_window_focused(focused);
        if self.refresh.observe_focus(focused) {
            self.chat.on_window_focus_gained();
        }
        let explicit = ctx.input(|input| {
            input.key_pressed(egui::Key::F5)
                || (input.modifiers.command && input.key_pressed(egui::Key::R))
        });
        if explicit {
            self.refresh.signal(RefreshTrigger::Explicit);
        }
    }

    /// Sends at most one opt-in read receipt for the open chat, using the
    /// same single focus signal as refresh. Its outcome is handled silently.
    fn send_read_receipt_if_due(&mut self) {
        if !self.transport.is_microsoft365_active() {
            return;
        }
        let Some(chat) = self.chat.take_mark_read_request(self.refresh.focused()) else {
            return;
        };
        if let Some(intent_id) = self
            .transport
            .enqueue([crate::integrations::WorkspaceCommand::MarkChatRead { chat }])
            .into_iter()
            .next()
        {
            self.read_receipt_intents.push(intent_id);
        }
    }

    /// Consumes the outcome of a read receipt. A receipt is never retried
    /// or reconciled; only a `403` is remembered, for the session.
    fn finish_read_receipt(&mut self, outcome: &CommandDispatch) -> bool {
        let (intent_id, error) = match outcome {
            CommandDispatch::Delivered { intent_id, .. } => (*intent_id, None),
            CommandDispatch::Rejected { intent_id, error }
            | CommandDispatch::DeliveryUncertain { intent_id, error } => (*intent_id, Some(error)),
            CommandDispatch::RetryScheduled { intent_id, .. }
            | CommandDispatch::ReconciliationCompleted { intent_id }
            | CommandDispatch::ReconciliationFailed { intent_id, .. } => {
                return self.read_receipt_intents.contains(intent_id);
            }
        };
        let Some(position) = self
            .read_receipt_intents
            .iter()
            .position(|candidate| *candidate == intent_id)
        else {
            return false;
        };
        self.read_receipt_intents.swap_remove(position);
        if matches!(
            error,
            Some(crate::integrations::TransportError::Unauthorized)
        ) {
            self.chat.mark_read_forbidden();
            self.toast.show_error(
                "Microsoft 365 did not allow read receipts, so they are off for this session.",
            );
        }
        true
    }

    /// Consumes the outcome of a chat-attachment upload command. The composer
    /// that asked gets the opaque handle, the progress of a session upload, or
    /// the reason it failed. An upload command is never retried here, and a
    /// `403` turns attaching off for the session. A fragment that reports
    /// progress for a composer that is gone cancels its session.
    fn finish_chat_upload(&mut self, outcome: &CommandDispatch) -> bool {
        use crate::features::chat::RemoteUploadOutcome;
        use crate::integrations::UploadReceipt;
        let tracked = |app: &Self, intent_id: &crate::integrations::WorkspaceIntentId| {
            app.upload_intents
                .iter()
                .any(|upload| upload.intent == *intent_id)
        };
        let (intent_id, result) = match outcome {
            CommandDispatch::Delivered { intent_id, receipt } => (
                *intent_id,
                match &receipt.upload {
                    Some(UploadReceipt::Done(handle)) => {
                        Some(RemoteUploadOutcome::Uploaded(handle.clone()))
                    }
                    Some(UploadReceipt::Progress(progress)) => {
                        Some(RemoteUploadOutcome::Progress(*progress))
                    }
                    // Only a share command reports this, and it is handled
                    // below before the composer is told anything.
                    Some(UploadReceipt::Shared(_)) => None,
                    None => None,
                },
            ),
            CommandDispatch::Rejected { intent_id, error }
            | CommandDispatch::DeliveryUncertain { intent_id, error } => (
                *intent_id,
                Some(crate::features::chat::upload_failure_reason(error)),
            ),
            CommandDispatch::RetryScheduled {
                intent_id,
                notify_user,
                ..
            } => {
                if !tracked(self, intent_id) {
                    return false;
                }
                if *notify_user {
                    self.toast.show_error(
                        "Microsoft 365 will retry the file upload after the provider backoff.",
                    );
                }
                return true;
            }
            CommandDispatch::ReconciliationCompleted { intent_id }
            | CommandDispatch::ReconciliationFailed { intent_id, .. } => {
                return tracked(self, intent_id);
            }
        };
        let Some(position) = self
            .upload_intents
            .iter()
            .position(|upload| upload.intent == intent_id)
        else {
            return false;
        };
        let upload = self.upload_intents.swap_remove(position);
        if upload.share {
            self.report_share(outcome);
            return true;
        }
        if upload.cancel {
            // A cancellation is best effort and says nothing.
            return true;
        }
        let result = result.unwrap_or(RemoteUploadOutcome::Failed(
            "The upload finished without a file. Pick it again.",
        ));
        if matches!(
            outcome,
            CommandDispatch::Rejected {
                error: crate::integrations::TransportError::Unauthorized,
                ..
            }
        ) {
            self.chat.mark_file_attach_forbidden();
        }
        let owned = self.chat.finish_remote_upload(intent_id, &result);
        if let RemoteUploadOutcome::Progress(progress) = &result {
            if owned {
                self.upload_progressed = true;
            } else {
                // Nobody waits for this session any more: discard it.
                self.enqueue_upload_cancel(upload.chat, progress.session());
            }
            return true;
        }
        match &result {
            RemoteUploadOutcome::Uploaded(handle) if handle.is_recovered() => self.toast.show(
                "The file may already be in OneDrive. Choose Attach to use it.",
            ),
            RemoteUploadOutcome::Uploaded(_) => self.toast.show("File uploaded. Send to share it."),
            RemoteUploadOutcome::NameConflict => self.toast.show_error(
                "A file with that name is already in your Teams chat files. Rename it and pick it again.",
            ),
            RemoteUploadOutcome::Failed(reason) => self.toast.show_error(*reason),
            RemoteUploadOutcome::Progress(_) | RemoteUploadOutcome::Cancelled => {}
        }
        true
    }

    /// Says how sharing a file with the chat's members went. Sharing never
    /// blocks the message, so a shortfall is only a warning: those people may
    /// not be able to open the file.
    fn report_share(&mut self, outcome: &CommandDispatch) {
        use crate::integrations::UploadReceipt;
        let warning = match outcome {
            CommandDispatch::Delivered { receipt, .. } => match &receipt.upload {
                Some(UploadReceipt::Shared(report)) if report.failed == 0 => return,
                Some(UploadReceipt::Shared(report)) if report.granted == 0 => {
                    "The file could not be shared with the people in this chat, so they may not be able to open it."
                        .to_owned()
                }
                Some(UploadReceipt::Shared(report)) => format!(
                    "The file could not be shared with {} of {} people in this chat, so they may not be able to open it.",
                    report.failed,
                    report.failed + report.granted
                ),
                _ => return,
            },
            _ => "The file could not be shared with the people in this chat, so they may not be able to open it."
                .to_owned(),
        };
        self.toast.show_error(warning);
    }

    /// Queues a best-effort cancellation of a session upload.
    fn enqueue_upload_cancel(
        &mut self,
        chat: crate::integrations::ProviderResourceRef,
        session: crate::integrations::UploadSessionRef,
    ) {
        if let Some(intent) = self
            .transport
            .enqueue([crate::integrations::WorkspaceCommand::CancelChatUpload {
                chat: chat.clone(),
                session,
            }])
            .into_iter()
            .next()
        {
            self.upload_intents.push(UploadIntent {
                intent,
                chat,
                cancel: true,
                share: false,
            });
        }
    }

    /// Queues the next fragment of a session upload and returns its intent.
    fn enqueue_upload_fragment(
        &mut self,
        chat: crate::integrations::ProviderResourceRef,
        session: crate::integrations::UploadSessionRef,
    ) -> crate::integrations::WorkspaceIntentId {
        let intent = self
            .transport
            .enqueue([crate::integrations::WorkspaceCommand::ContinueChatUpload {
                chat: chat.clone(),
                session,
            }])
            .into_iter()
            .next()
            .expect("enqueuing one upload fragment must allocate one intent id");
        self.upload_intents.push(UploadIntent {
            intent,
            chat,
            cancel: false,
            share: false,
        });
        intent
    }

    /// A selection staged its own read: record it so a nearby event does not
    /// read the same target again, and let the directory follow the visit.
    fn note_selection_read(&mut self, target: RefreshTarget) {
        let now = std::time::Instant::now();
        self.refresh.note_read(target, now);
        self.refresh.signal(RefreshTrigger::SelectionOpened);
    }

    /// Runs the reads the pending refresh event allows. Everything goes
    /// through the controller's own bounded entry points, so a refresh never
    /// cancels a delivery or reconciliation read and is a no-op while the
    /// same kind is already busy.
    fn drive_remote_refresh(&mut self, ctx: &egui::Context) {
        if !self.transport.is_microsoft365_active() {
            self.refresh.reset();
            return;
        }
        let selected: &[RefreshTarget] = match self.nav.section {
            Section::Chat => &[RefreshTarget::SelectedChat],
            Section::Teams => &[
                RefreshTarget::SelectedChannel,
                RefreshTarget::SelectedReplies,
            ],
            _ => &[],
        };
        let plan = self.refresh.take_plan(std::time::Instant::now(), selected);
        if plan.is_empty() {
            return;
        }
        for target in plan.targets() {
            match target {
                RefreshTarget::Directory => {
                    self.transport.request_remote_chats_refresh();
                }
                RefreshTarget::SelectedChat => {
                    if let Some(chat) = self.chat.begin_selected_remote_history_refresh() {
                        self.transport.request_remote_chat_messages(chat);
                    }
                }
                RefreshTarget::SelectedChannel => {
                    if let Some((team, channel)) =
                        self.workspace.begin_remote_channel_messages_refresh()
                    {
                        self.transport
                            .request_remote_channel_messages(team, channel);
                    }
                }
                RefreshTarget::SelectedReplies => {
                    if let Some((team, channel, post)) =
                        self.workspace.begin_remote_channel_replies_refresh()
                    {
                        self.transport
                            .request_remote_channel_replies(team, channel, post);
                    }
                }
            }
        }
        // The worker wakes the UI when it finishes; this only starts it.
        ctx.request_repaint_after(std::time::Duration::ZERO);
    }

    /// Receives a completed browser sign-in without ever inspecting its
    /// credentials. The move-only token object passes directly into the
    /// integration-owned Graph transport constructor.
    fn collect_remote_authorization(&mut self, ctx: &egui::Context) {
        if let Some(outcome) = self.remote_auth.poll() {
            match outcome {
                Ok(tokens) => match self.transport.connect_microsoft365(tokens) {
                    Ok(()) => {
                        self.chat.clear_remote_projection();
                        self.workspace.clear_remote_teams_directory();
                        self.workspace.clear_remote_calendar_directory();
                        self.workspace.clear_remote_files_directory();
                        self.workspace
                            .forget_discarded_remote_file_preview_images(ctx);
                        self.toast.show(
                            "Microsoft 365 authorization completed. Verifying the connected identity…",
                        );
                    }
                    Err(error) => self.toast.show_error(format!(
                        "Microsoft 365 authorization completed, but the connection could not start ({error})."
                    )),
                },
                Err(error) => self
                    .toast
                    .show_error(format!("Microsoft 365 sign-in did not complete ({error}).")),
            }
        }
        if self.remote_auth.is_authorizing() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn start_microsoft365_sign_in(&mut self) {
        match self.remote_auth.start() {
            Ok(()) => self.toast.show(
                "Your browser was opened for Microsoft 365 sign-in. Finish there, then return to Teamspace.",
            ),
            Err(error) => self
                .toast
                .show_error(format!("Microsoft 365 sign-in could not start ({error}).")),
        }
    }

    fn cancel_microsoft365_sign_in(&mut self) {
        self.remote_auth.cancel();
        self.toast.show("Microsoft 365 sign-in was cancelled.");
    }

    fn disconnect_microsoft365(&mut self, ctx: &egui::Context) {
        self.remote_auth.cancel();
        self.transport.disconnect_microsoft365();
        self.chat.clear_remote_projection();
        self.workspace.clear_remote_teams_directory();
        self.workspace.clear_remote_calendar_directory();
        self.workspace.clear_remote_files_directory();
        self.workspace
            .forget_discarded_remote_file_preview_images(ctx);
        self.toast
            .show("Microsoft 365 was disconnected. Local preview data remains on this device.");
    }

    fn flush_workspace_transport(&mut self, ctx: &egui::Context) {
        // A remote-calendar Join meeting click is an explicit, one-shot browser
        // handoff rather than a Graph write, local call, or transport intent.
        // Revalidate the provider-normalized link at this final boundary and
        // never include its opaque URL in a toast or log-like UI string.
        for result in launch_remote_browser_handoff_links(
            self.workspace.take_remote_meeting_join_links(),
            crate::app::browser::open_https_url,
        ) {
            let failed = result.is_err();
            let text = remote_meeting_join_toast(result);
            if failed {
                self.toast.show_error(text);
            } else {
                self.toast.show(text);
            }
        }
        // A remote OneDrive row can request this same explicit, one-shot
        // handoff. It remains a browser action rather than a Graph read or
        // mutation, and its opaque URL is revalidated only at this final
        // boundary without entering a toast or log-like UI string.
        for result in launch_remote_browser_handoff_links(
            self.workspace.take_remote_file_open_links(),
            crate::app::browser::open_https_url,
        ) {
            let failed = result.is_err();
            let text = remote_file_open_toast(result);
            if failed {
                self.toast.show_error(text);
            } else {
                self.toast.show(text);
            }
        }
        // A link clicked in a formatted chat message is the same explicit,
        // one-shot handoff. The feature only queues the destination text; it
        // is revalidated here and never enters a toast.
        for result in launch_remote_browser_handoff_links(
            self.chat.take_remote_link_opens(),
            crate::app::browser::open_https_url,
        ) {
            match remote_chat_link_toast(result) {
                Ok(message) => self.toast.show(message),
                Err(message) => self.toast.show_error(message),
            }
        }
        // A remote file preview is a bounded selected read, not a browser
        // handoff or mutation. Drain the feature-owned cancellation first so
        // replacing a file can stop only its obsolete worker before staging
        // the newer target.
        if self
            .workspace
            .take_remote_file_preview_cancellation_requested()
        {
            self.transport.cancel_remote_file_preview();
        }
        if let Some(item) = self.workspace.take_remote_file_preview_request() {
            self.transport.request_remote_file_preview(item);
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        // A selected remote channel-file row uses the same final browser
        // boundary, but its feature state and provider read remain separate
        // from the signed-in user's OneDrive root. No provider URL enters a
        // toast or a transport command.
        for result in launch_remote_browser_handoff_links(
            self.workspace.take_remote_channel_file_open_links(),
            crate::app::browser::open_https_url,
        ) {
            let failed = result.is_err();
            let text = remote_file_open_toast(result);
            if failed {
                self.toast.show_error(text);
            } else {
                self.toast.show(text);
            }
        }
        // Leaving a selected remote chat must stop only its bounded history
        // read. It cannot cancel the account-wide chat projection that drives
        // directory refresh or any in-flight delivery reconciliation.
        if self.chat.take_remote_chat_messages_cancellation_requested() {
            self.transport.cancel_remote_chat_messages();
        }
        if let Some(chat) = self.chat.take_remote_chat_messages_request() {
            self.note_selection_read(RefreshTarget::SelectedChat);
            self.transport.request_remote_chat_messages(chat);
            // Sidebar selection occurs after the normal synchronization pass;
            // ask for a prompt frame so this bounded read can start promptly.
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        // The contextual pinned-message panel owns a separate selected-chat
        // read. Its close or selection replacement must not disturb main
        // history, delivery reconciliation, or account-wide sync.
        if self
            .chat
            .take_remote_chat_pinned_messages_cancellation_requested()
        {
            self.transport.cancel_remote_chat_pinned_messages();
        }
        if let Some(chat) = self.chat.take_remote_chat_pinned_messages_request() {
            self.transport.request_remote_chat_pinned_messages(chat);
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        // The selected chat's People roster is its own contextual read. Drain
        // closure/replacement before issuing a new target so it cannot affect
        // history, pins, delivery reconciliation, or global synchronization.
        if self.chat.take_remote_chat_members_cancellation_requested() {
            self.transport.cancel_remote_chat_members();
            self.transport.cancel_remote_chat_member_presence();
        }
        if let Some(chat) = self.chat.take_remote_chat_members_request() {
            self.transport.request_remote_chat_members(chat);
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        // Presence refresh: only while People is open and the window is
        // focused, at most every 60 s, and never a roster re-read. It uses
        // the same single focus signal as the event-driven refresh.
        let focused = self.refresh.focused();
        match self
            .chat
            .remote_chat_member_presence_schedule(std::time::Instant::now(), focused)
        {
            crate::features::chat::PresenceSchedule::Request(chat) => {
                self.transport.request_remote_chat_member_presence(chat);
                ctx.request_repaint_after(std::time::Duration::ZERO);
            }
            crate::features::chat::PresenceSchedule::Wait(remaining) => {
                ctx.request_repaint_after(remaining);
            }
            crate::features::chat::PresenceSchedule::Idle => {}
        }
        // Changing or clearing a selected remote channel can leave a backing
        // folder resolution queued or in flight. Drain that feature-owned
        // signal independently so it cannot disturb posts, replies, global
        // synchronization, or command delivery.
        if self
            .workspace
            .take_remote_channel_files_cancellation_requested()
        {
            self.transport.cancel_remote_channel_files();
        }
        // A feature transition can drop the selected root post while its
        // bounded reply read is queued or in flight. Drain that one-shot
        // lifecycle signal before staging replacement reads; this controller
        // helper affects only `ChannelReplies`, never channel posts, global
        // sync, or mutation delivery.
        if self
            .workspace
            .take_remote_channel_replies_cancellation_requested()
        {
            self.transport.cancel_remote_channel_replies();
        }
        if let Some((team, channel)) = self.workspace.take_remote_channel_messages_request() {
            self.note_selection_read(RefreshTarget::SelectedChannel);
            self.transport
                .request_remote_channel_messages(team, channel);
            // Sidebar selection occurs after the normal synchronization pass;
            // request one prompt follow-up frame so the controller can start
            // the bounded remote poll without waiting for unrelated input.
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        if let Some((team, channel)) = self.workspace.take_remote_channel_files_request() {
            self.transport.request_remote_channel_files(team, channel);
            // Opening the remote Files surface happens after normal polling;
            // ask for a prompt frame so its bounded two-step read can start.
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        if let Some((team, channel, post)) = self.workspace.take_remote_channel_replies_request() {
            self.note_selection_read(RefreshTarget::SelectedReplies);
            self.transport
                .request_remote_channel_replies(team, channel, post);
            // A root-post selection occurs after the normal synchronization
            // pass, just like channel selection. Ask for a prompt frame so
            // the separate bounded reply read can start immediately.
            ctx.request_repaint_after(std::time::Duration::ZERO);
        }
        self.drive_remote_refresh(ctx);
        self.send_read_receipt_if_due();
        let _ = self
            .transport
            .enqueue(self.workspace.take_transport_commands());
        for (submission, team, channel, body) in self.workspace.take_remote_channel_post_requests()
        {
            // Remote channel root posts are feature-local until this narrow
            // handoff. Enqueue exactly the provider-neutral mutation the
            // adapter understands, then retain its controller intent for
            // delivery outcomes; never project the text into local Teams
            // persistence or a speculative remote feed row.
            let intent_id = self
                .transport
                .enqueue([crate::integrations::WorkspaceCommand::SendChannelPost {
                    team,
                    channel,
                    body,
                }])
                .into_iter()
                .next()
                .expect("enqueuing one remote channel post must allocate one intent id");
            let _ = self
                .workspace
                .bind_remote_channel_post_submission(submission, intent_id);
        }
        for (submission, team, channel, post, body) in
            self.workspace.take_remote_channel_reply_requests()
        {
            // A remote root reply is feature-local until this narrow handoff.
            // Preserve its exact provider-issued root target, bind only the
            // controller intent, and never write a speculative reply through
            // ThreadStore or local Teams persistence.
            let intent_id = self
                .transport
                .enqueue([crate::integrations::WorkspaceCommand::ReplyToChannelPost {
                    team,
                    channel,
                    post,
                    body,
                }])
                .into_iter()
                .next()
                .expect("enqueuing one remote channel reply must allocate one intent id");
            let _ = self
                .workspace
                .bind_remote_channel_reply_submission(submission, intent_id);
        }
        for (team, channel, post, reaction, selected) in
            self.workspace.take_remote_channel_post_reaction_requests()
        {
            // A remote root-post reaction has an exact provider route, but
            // no local optimistic reaction overlay. Bind the controller ID
            // back to the feature's opaque target gate so only a known
            // rejection can unlock it before the follow-up feed arrives.
            let intent_id = self
                .transport
                .enqueue([
                    crate::integrations::WorkspaceCommand::SetChannelMessageReaction {
                        team: team.clone(),
                        channel: channel.clone(),
                        post: post.clone(),
                        reaction,
                        selected,
                    },
                ])
                .into_iter()
                .next()
                .expect("enqueuing one remote channel reaction must allocate one intent id");
            if !self
                .workspace
                .bind_remote_channel_post_reaction(intent_id, team, channel, post, reaction)
            {
                // `dispatch_due` runs after this handoff loop. If an
                // unexpected lifecycle transition drops the feature gate in
                // this tiny interval, cancel the still-unstarted command so
                // it fails closed instead of writing an orphan provider route.
                self.transport.cancel_queued_remote_intents([intent_id]);
            }
        }
        for (team, channel, post, reply, reaction, selected) in
            self.workspace.take_remote_channel_reply_reaction_requests()
        {
            // A selected-root reply reaction has one more exact route segment
            // than a root-post reaction. Preserve all opaque targets until
            // the provider-neutral command receives a controller intent, so
            // only the matching reply gate can later resolve an outcome.
            let intent_id = self
                .transport
                .enqueue([
                    crate::integrations::WorkspaceCommand::SetChannelMessageReplyReaction {
                        team: team.clone(),
                        channel: channel.clone(),
                        post: post.clone(),
                        reply: reply.clone(),
                        reaction,
                        selected,
                    },
                ])
                .into_iter()
                .next()
                .expect("enqueuing one remote channel reply reaction must allocate one intent id");
            if !self
                .workspace
                .bind_remote_channel_reply_reaction(intent_id, team, channel, post, reply, reaction)
            {
                // See the root-post reaction handoff above. The full reply
                // route is cancelled before dispatch if its gate disappeared
                // after enqueueing but before intent binding.
                self.transport.cancel_queued_remote_intents([intent_id]);
            }
        }
        for action in self.chat.take_actions() {
            match action {
                ChatAction::SendLocalMessage { chat_id, body } => {
                    let _ = self.transport.enqueue([
                        crate::integrations::WorkspaceCommand::SendChatMessage {
                            chat: crate::integrations::ProviderResourceRef::local_preview(chat_id),
                            body: crate::integrations::ComposedBody::plain(&body),
                            importance: crate::integrations::RemoteChatMessageImportance::Normal,
                            attachment: None,
                        },
                    ]);
                }
                ChatAction::UploadRemoteAttachment {
                    submission,
                    chat,
                    source,
                } => {
                    let upload_chat = chat.clone();
                    let intent_id = self
                        .transport
                        .enqueue([crate::integrations::WorkspaceCommand::UploadChatFile {
                            chat,
                            source,
                        }])
                        .into_iter()
                        .next()
                        .expect("enqueuing one upload must allocate one intent id");
                    self.upload_intents.push(UploadIntent {
                        intent: intent_id,
                        chat: upload_chat,
                        cancel: false,
                        share: false,
                    });
                    if !self.chat.bind_remote_upload(submission, intent_id) {
                        // The composer vanished between the drain and this
                        // handoff: never let an orphan upload reach OneDrive.
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
                ChatAction::ContinueRemoteUpload {
                    submission,
                    chat,
                    session,
                } => {
                    let intent_id = self.enqueue_upload_fragment(chat.clone(), session);
                    if !self.chat.bind_remote_upload(submission, intent_id) {
                        // The composer is gone: do not send the fragment, and
                        // discard the session.
                        self.transport.cancel_queued_remote_intents([intent_id]);
                        self.enqueue_upload_cancel(chat, session);
                    }
                }
                ChatAction::CancelRemoteUpload {
                    chat,
                    session,
                    pending_intent,
                } => {
                    if let Some(intent) = pending_intent {
                        // A fragment that has not started never will.
                        self.transport.cancel_queued_remote_intents([intent]);
                    }
                    if let Some(session) = session {
                        self.enqueue_upload_cancel(chat, session);
                    }
                }
                ChatAction::SendRemoteMessage {
                    submission,
                    chat,
                    body,
                    importance,
                    attachment,
                } => {
                    // A file is shared with the chat's members just before the
                    // message that links it. The share is queued first, so it
                    // runs first, and its failure never stops the message.
                    if let Some(handle) = &attachment
                        && let Some(share) = self
                            .transport
                            .enqueue([
                                crate::integrations::WorkspaceCommand::ShareUploadedFileWithChat {
                                    chat: chat.clone(),
                                    upload: handle.clone(),
                                },
                            ])
                            .into_iter()
                            .next()
                    {
                        self.upload_intents.push(UploadIntent {
                            intent: share,
                            chat: chat.clone(),
                            cancel: false,
                            share: true,
                        });
                    }
                    let intent_id = self
                        .transport
                        .enqueue([crate::integrations::WorkspaceCommand::SendChatMessage {
                            chat,
                            body,
                            importance,
                            attachment,
                        }])
                        .into_iter()
                        .next()
                        .expect("enqueuing one remote chat action must allocate one intent id");
                    if !self.chat.bind_remote_submission(submission, intent_id) {
                        // A stale feature action can disappear between its
                        // one-shot drain and this controller handoff. Cancel
                        // the still-unstarted remote write rather than let an
                        // orphan opaque route reach Graph.
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
                ChatAction::ReplyToRemoteChatMessageWithQuote {
                    submission,
                    chat,
                    messages,
                    body,
                } => {
                    // The feature retains every opaque target from the exact
                    // selected-history rows. This handoff neither derives a
                    // route from text nor permits a generic local reply.
                    let intent_id = self
                        .transport
                        .enqueue([
                            crate::integrations::WorkspaceCommand::ReplyToChatMessageWithQuote {
                                chat,
                                messages,
                                body,
                            },
                        ])
                        .into_iter()
                        .next()
                        .expect("enqueuing one remote quote reply must allocate one intent id");
                    if !self.chat.bind_remote_submission(submission, intent_id) {
                        // The source row may have disappeared after the
                        // action was drained. Keep quote replies fail-closed:
                        // an unbound opaque command must never reach Graph.
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
                ChatAction::EditRemoteChatMessage {
                    submission,
                    chat,
                    message,
                    body,
                } => {
                    // An edit is a PATCH that is never replayed. The feature
                    // keeps its text until the outcome is known.
                    let intent_id = self
                        .transport
                        .enqueue([crate::integrations::WorkspaceCommand::EditChatMessage {
                            chat,
                            message,
                            body,
                        }])
                        .into_iter()
                        .next()
                        .expect("enqueuing one remote chat edit must allocate one intent id");
                    if !self.chat.bind_remote_submission(submission, intent_id) {
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
                ChatAction::DeleteRemoteChatMessage {
                    submission,
                    chat,
                    message,
                } => {
                    let intent_id = self
                        .transport
                        .enqueue([crate::integrations::WorkspaceCommand::DeleteChatMessage {
                            chat,
                            message,
                        }])
                        .into_iter()
                        .next()
                        .expect("enqueuing one remote chat deletion must allocate one intent id");
                    if !self.chat.bind_remote_submission(submission, intent_id) {
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
                ChatAction::SetRemoteChatMessageReaction {
                    chat,
                    message,
                    reaction,
                    selected,
                } => {
                    // A selected remote chat reaction must retain both opaque
                    // provider targets until it receives the controller's
                    // intent. The feature uses that binding to unlock only
                    // this exact message after a known rejection.
                    let intent_id = self
                        .transport
                        .enqueue([
                            crate::integrations::WorkspaceCommand::SetChatMessageReaction {
                                chat: chat.clone(),
                                message: message.clone(),
                                reaction,
                                selected,
                            },
                        ])
                        .into_iter()
                        .next()
                        .expect("enqueuing one remote chat reaction must allocate one intent id");
                    if !self
                        .chat
                        .bind_remote_chat_reaction(intent_id, chat, message, reaction)
                    {
                        // `dispatch_due` follows this loop. A vanished chat
                        // history gate must fail closed before the freshly
                        // enqueued remote mutation can reach Graph.
                        self.transport.cancel_queued_remote_intents([intent_id]);
                    }
                }
            }
        }

        for outcome in self.transport.dispatch_due() {
            if self.finish_read_receipt(&outcome) {
                continue;
            }
            if self.finish_chat_upload(&outcome) {
                continue;
            }
            match outcome {
                CommandDispatch::Delivered { intent_id, receipt } => {
                    if self.chat.remote_message_mutation_delivered(intent_id) {
                        self.toast.show(
                            "Microsoft 365 accepted the change. Refreshing the conversation…",
                        );
                        continue;
                    }
                    let resolved_remote_send = self.chat.remote_send_delivered(intent_id);
                    if resolved_remote_send {
                        self.toast
                            .show("Message sent to Microsoft 365. Refreshing the conversation…");
                        continue;
                    }
                    if self.chat.remote_chat_reaction_owned(intent_id) {
                        self.toast.show(
                            "Microsoft 365 updated the message reaction. Refreshing that conversation…",
                        );
                        continue;
                    }
                    if self.workspace.remote_channel_post_delivered(intent_id) {
                        self.toast
                            .show("Channel post sent to Microsoft 365. Refreshing that channel…");
                        continue;
                    }
                    if self.workspace.remote_channel_reply_delivered(intent_id) {
                        self.toast.show(
                            "Channel reply sent to Microsoft 365. Refreshing that reply thread…",
                        );
                        continue;
                    }
                    if self.workspace.remote_channel_post_reaction_owned(intent_id) {
                        self.toast
                            .show("Microsoft 365 updated the reaction. Refreshing that channel…");
                        continue;
                    }
                    if self
                        .workspace
                        .remote_channel_reply_reaction_owned(intent_id)
                    {
                        self.toast.show(
                            "Microsoft 365 updated the reply reaction. Refreshing that reply thread…",
                        );
                        continue;
                    }
                    if !matches!(
                        receipt.disposition,
                        crate::integrations::CommandDisposition::StagedLocally
                    ) {
                        self.toast
                            .show(WorkspaceTransportController::receipt_summary(&receipt));
                    }
                }
                CommandDispatch::RetryScheduled {
                    intent_id,
                    error,
                    notify_user,
                } => {
                    if self.chat.remote_message_mutation_owned(intent_id) {
                        if notify_user {
                            self.toast.show_error(format!(
                                "Microsoft 365 will retry the message change after the provider backoff ({error})."
                            ));
                        }
                        continue;
                    }
                    let remote_send = self.chat.remote_send_retry_scheduled(intent_id);
                    let remote_chat_reaction = if remote_send {
                        false
                    } else {
                        self.chat.remote_chat_reaction_owned(intent_id)
                    };
                    let remote_channel_post = if remote_send || remote_chat_reaction {
                        false
                    } else {
                        self.workspace
                            .remote_channel_post_retry_scheduled(intent_id)
                    };
                    let remote_channel_reply =
                        if remote_send || remote_chat_reaction || remote_channel_post {
                            false
                        } else {
                            self.workspace
                                .remote_channel_reply_retry_scheduled(intent_id)
                        };
                    let remote_channel_reaction = if remote_send
                        || remote_chat_reaction
                        || remote_channel_post
                        || remote_channel_reply
                    {
                        false
                    } else {
                        self.workspace.remote_channel_post_reaction_owned(intent_id)
                    };
                    let remote_channel_reply_reaction = if remote_send
                        || remote_chat_reaction
                        || remote_channel_post
                        || remote_channel_reply
                        || remote_channel_reaction
                    {
                        false
                    } else {
                        self.workspace
                            .remote_channel_reply_reaction_owned(intent_id)
                    };
                    if notify_user {
                        self.toast.show_error(if remote_send {
                            format!(
                                "Microsoft 365 message delivery will retry after the provider backoff ({error})."
                            )
                        } else if remote_chat_reaction {
                            format!(
                                "Microsoft 365 message reaction update will retry after the provider backoff ({error})."
                            )
                        } else if remote_channel_post {
                            format!(
                                "Microsoft 365 channel post delivery will retry after the provider backoff ({error})."
                            )
                        } else if remote_channel_reply {
                            format!(
                                "Microsoft 365 channel reply delivery will retry after the provider backoff ({error})."
                            )
                        } else if remote_channel_reaction {
                            format!(
                                "Microsoft 365 reaction update will retry after the provider backoff ({error})."
                            )
                        } else if remote_channel_reply_reaction {
                            format!(
                                "Microsoft 365 reply reaction update will retry after the provider backoff ({error})."
                            )
                        } else {
                            format!(
                                "The local change was saved; its workspace intent will retry ({error})."
                            )
                        });
                    }
                }
                CommandDispatch::Rejected { intent_id, error } => {
                    let forbidden =
                        matches!(&error, crate::integrations::TransportError::Unauthorized);
                    if let Some(label) = self
                        .chat
                        .remote_message_mutation_rejected(intent_id, forbidden)
                    {
                        self.toast
                            .show_error(remote_mutation_rejected_toast(label, forbidden, &error));
                    } else if self.chat.remote_send_failed(intent_id) {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that chat in the current sync; its unsent draft is retained until the chat returns."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the message; its draft is ready to edit ({error})."
                            )
                        });
                    } else if self.chat.remote_chat_reaction_failed(intent_id) {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that chat; its reaction control is available again."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the message reaction update; the control is available again ({error})."
                            )
                        });
                    } else if self.workspace.remote_channel_post_failed(intent_id) {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that channel; its unsent post draft is retained until the channel returns."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the channel post; its draft is ready to edit ({error})."
                            )
                        });
                    } else if self.workspace.remote_channel_reply_failed(intent_id) {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that reply thread; its unsent reply draft is retained until the root post returns."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the channel reply; its draft is ready to edit ({error})."
                            )
                        });
                    } else if self
                        .workspace
                        .remote_channel_post_reaction_failed(intent_id)
                    {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that channel; its reaction control is available again."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the reaction update; the control is available again ({error})."
                            )
                        });
                    } else if self
                        .workspace
                        .remote_channel_reply_reaction_failed(intent_id)
                    {
                        self.toast.show_error(if matches!(
                            &error,
                            crate::integrations::TransportError::Cancelled
                        ) {
                            "Microsoft 365 no longer included that reply thread; its reaction control is available again."
                                .into()
                        } else {
                            format!(
                                "Microsoft 365 did not accept the reply reaction update; the control is available again ({error})."
                            )
                        });
                    } else {
                        self.toast.show_error(format!(
                            "The local change was saved, but an invalid workspace intent was discarded ({error})."
                        ));
                    }
                }
                CommandDispatch::DeliveryUncertain { intent_id, error } => {
                    if self
                        .chat
                        .remote_message_mutation_delivery_uncertain(intent_id)
                    {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm the message change. Checking the chat before allowing another change ({error})."
                        ));
                    } else if self.chat.remote_send_delivery_uncertain(intent_id) {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm delivery. Checking the chat before allowing a retry ({error})."
                        ));
                    } else if self.chat.remote_chat_reaction_owned(intent_id) {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm the message reaction update. Refreshing that conversation before another change ({error})."
                        ));
                    } else if self
                        .workspace
                        .remote_channel_post_delivery_uncertain(intent_id)
                    {
                        self.toast
                            .show_error(remote_channel_post_delivery_uncertain_toast(
                                self.transport.is_microsoft365_active(),
                                &error,
                            ));
                    } else if self
                        .workspace
                        .remote_channel_reply_delivery_uncertain(intent_id)
                    {
                        self.toast
                            .show_error(remote_channel_reply_delivery_uncertain_toast(
                                self.transport.is_microsoft365_active(),
                                &error,
                            ));
                    } else if self.workspace.remote_channel_post_reaction_owned(intent_id) {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm the reaction update. Refreshing that channel before another change ({error})."
                        ));
                    } else if self
                        .workspace
                        .remote_channel_reply_reaction_owned(intent_id)
                    {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm the reply reaction update. Refreshing that reply thread before another change ({error})."
                        ));
                    } else {
                        self.toast.show_error(format!(
                            "Workspace delivery could not be confirmed; review the change before retrying ({error})."
                        ));
                    }
                }
                CommandDispatch::ReconciliationCompleted { intent_id } => {
                    if let Some(resolution) = self
                        .chat
                        .complete_remote_message_mutation_reconciliation(intent_id)
                    {
                        self.toast
                            .show(remote_mutation_resolution_toast(resolution));
                        continue;
                    }
                    match self.chat.complete_remote_send_reconciliation(intent_id) {
                        Some(crate::features::chat::RemoteReconciliationResolution::Confirmed) => {
                            self.toast.show(
                                "Microsoft 365 confirmed the uncertain message in the refreshed chat.",
                            );
                        }
                        Some(
                            crate::features::chat::RemoteReconciliationResolution::DraftRestored,
                        ) => {
                            self.toast.show_error(
                                "Microsoft 365 did not show the uncertain message; its draft is ready for an explicit retry.",
                            );
                        }
                        None => {}
                    }
                }
                CommandDispatch::ReconciliationFailed { intent_id, error } => {
                    if let Some(label) = self
                        .chat
                        .fail_remote_message_mutation_reconciliation(intent_id)
                    {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not confirm that {}; it is ready to review ({error}).",
                            mutation_noun(label)
                        ));
                    } else if self.chat.fail_remote_send_reconciliation(intent_id) {
                        self.toast.show_error(format!(
                            "Microsoft 365 could not reconcile that message; its draft is ready to review ({error})."
                        ));
                    }
                }
            }
        }
        if std::mem::take(&mut self.upload_progressed) {
            // The next fragment's command is drained at the top of a frame.
            ctx.request_repaint();
        }
        if self.transport.is_polling() {
            // A remote worker can begin during this flush, after the earlier
            // synchronization pass has already checked its state. Keep that
            // first wait bounded without turning an in-flight FIFO head into
            // a zero-delay repaint loop. Completion wakes the UI directly.
            ctx.request_repaint_after(REMOTE_WORK_REPAINT_FALLBACK);
        } else if let Some(delay) = self.transport.next_retry_delay() {
            ctx.request_repaint_after(delay);
        }
    }

    fn queue_local_storage_notice(&mut self, notice: String) {
        if let Some(existing) = &mut self.pending_local_storage_notice {
            existing.push(' ');
            existing.push_str(&notice);
        } else {
            self.pending_local_storage_notice = Some(notice);
        }
    }

    fn draw_topbar(&mut self, ctx: &egui::Context, global_search_enabled: bool) -> bool {
        let mut search_input_focused = false;
        let navigation_enabled = self.navigation_enabled();
        if !global_search_enabled {
            self.search.dismiss();
        }
        egui::TopBottomPanel::top("topbar")
            .exact_height(TOPBAR_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(theme::topbar_tint())
                    .inner_margin(egui::Margin::symmetric(
                        theme::space::LG as i8,
                        theme::space::SM as i8,
                    ))
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
            )
            .show(ctx, |ui| {
                let panel_center_x = ui.max_rect().center().x;
                ui.horizontal(|ui| {
                    let sidebar_tooltip = if self.sidebar_collapsed {
                        "Show sidebar"
                    } else {
                        "Hide sidebar"
                    };
                    if chrome_icon_button(ui, Icon::PanelLeft, navigation_enabled, sidebar_tooltip)
                        .clicked()
                    {
                        self.sidebar_collapsed = !self.sidebar_collapsed;
                    }
                    if chrome_icon_button(
                        ui,
                        Icon::ChevronLeft,
                        navigation_enabled && self.can_go_back(),
                        "Back",
                    )
                    .clicked()
                    {
                        self.go_back();
                    }
                    if chrome_icon_button(
                        ui,
                        Icon::ChevronRight,
                        navigation_enabled && self.can_go_forward(),
                        "Forward",
                    )
                    .clicked()
                    {
                        self.go_forward();
                    }
                    if self.transport.is_microsoft365_active()
                        && chrome_icon_button(ui, Icon::Repeat, navigation_enabled, "Refresh (F5)")
                            .clicked()
                    {
                        self.refresh.signal(RefreshTrigger::Explicit);
                    }
                    let search_id = ui.make_persistent_id("global_search_input");
                    if global_search_enabled
                        && ctx.input(|input| {
                            input.modifiers.command && input.key_pressed(egui::Key::E)
                        })
                    {
                        ctx.memory_mut(|memory| memory.request_focus(search_id));
                        self.palette.close();
                        self.search.open();
                    }
                    let search_width = topbar_search_width(ui.available_width());
                    // Centre the search box on the bar while always leaving the
                    // account region its full width.
                    let max_gap = (ui.available_width()
                        - search_width
                        - TOPBAR_ACCOUNT_WIDTH
                        - TOPBAR_ITEM_SPACING)
                        .max(0.0);
                    let centring_gap = (panel_center_x - search_width / 2.0 - ui.cursor().left())
                        .clamp(0.0, max_gap);
                    ui.add_space(centring_gap + TOPBAR_ITEM_SPACING);
                    let search_response = ui
                        .add_enabled_ui(global_search_enabled, |ui| {
                            draw_search_box(ui, search_id, self.search.query_mut(), search_width)
                        })
                        .inner;
                    if global_search_enabled
                        && (search_response.changed() || search_response.gained_focus())
                    {
                        self.search.open();
                        self.toast.clear();
                    }
                    // Enter and Escape make a singleline TextEdit surrender focus on the
                    // frame they are pressed, so that frame counts as focused too.
                    search_input_focused = global_search_enabled
                        && (search_response.has_focus() || search_response.lost_focus());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_enabled_ui(navigation_enabled, |ui| self.draw_topbar_account(ui));
                    });
                });
            });
        search_input_focused
    }

    fn global_search_enabled(&self) -> bool {
        self.navigation_enabled() && !self.auth.is_profile_open()
    }

    fn navigation_enabled(&self) -> bool {
        !self.shortcuts_open
            && !self.palette.is_open()
            && !self.settings.is_open()
            && match self.nav.section {
                Section::Chat => !self.chat.has_keyboard_modal(),
                section => !self.workspace.has_keyboard_modal(section),
            }
    }

    /// Right-to-left: the signed-in avatar, the organization, then the
    /// overflow menu. The avatar and organization open the account menu
    /// (status and Sign out); the overflow menu holds Microsoft 365 actions.
    fn draw_topbar_account(&mut self, ui: &mut egui::Ui) {
        let presence = self.auth.local_presence();
        let side = AvatarSize::Md32.px();
        let (avatar_rect, avatar_response) =
            ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
        Avatar::new(self.auth.display_name(), AvatarSize::Md32)
            .presence(presence.avatar_presence())
            .paint(ui, avatar_rect);
        let tooltip = format!("{} (local preview) · Open account menu", presence.label());
        avatar_response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &tooltip)
        });
        let avatar_clicked = avatar_response.on_hover_text(tooltip).clicked();
        ui.add_space(theme::space::XS);
        let organization_clicked = ui
            .scope(|ui| {
                ui.set_max_width(TOPBAR_ORGANIZATION_WIDTH);
                ui.add(
                    egui::Label::new(
                        RichText::new(&workspace().organization)
                            .font(theme::regular(theme::font_size::BODY))
                            .color(theme::fg1()),
                    )
                    .truncate()
                    .sense(egui::Sense::click()),
                )
                .on_hover_text("Open account menu")
                .clicked()
            })
            .inner;
        if avatar_clicked || organization_clicked {
            self.auth.toggle_profile();
        }
        let mut start_microsoft365_sign_in = false;
        let mut cancel_microsoft365_sign_in = false;
        let mut disconnect_microsoft365 = false;
        let mut show_preview_about = false;
        let overflow = chrome_icon_button(ui, Icon::More, true, "More options");
        Popup::menu(&overflow).show(|ui| {
            ui.label(
                RichText::new(self.transport.status_label())
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
            ui.separator();
            if self.remote_auth.is_authorizing() {
                if ui.button("Cancel Microsoft 365 sign-in").clicked() {
                    cancel_microsoft365_sign_in = true;
                    ui.close();
                }
            } else if self.transport.is_microsoft365_active() {
                if ui.button("Disconnect Microsoft 365").clicked() {
                    disconnect_microsoft365 = true;
                    ui.close();
                }
            } else if ui.button("Connect Microsoft 365").clicked() {
                start_microsoft365_sign_in = true;
                ui.close();
            }
            ui.label(
                RichText::new(self.remote_auth.cloud_label())
                    .font(theme::regular(theme::font_size::MICRO))
                    .color(theme::fg3()),
            );
            ui.label(
                RichText::new(
                    "Requires a tenant-owned Entra app; Chat.Read loads chats and ChatMessage.Send enables text sends.",
                )
                .font(theme::regular(theme::font_size::MICRO))
                .color(theme::fg3()),
            );
            ui.separator();
            if ui.button("About this preview").clicked() {
                show_preview_about = true;
                ui.close();
            }
        });
        if start_microsoft365_sign_in {
            self.start_microsoft365_sign_in();
        }
        if cancel_microsoft365_sign_in {
            self.cancel_microsoft365_sign_in();
        }
        if disconnect_microsoft365 {
            self.disconnect_microsoft365(ui.ctx());
        }
        if show_preview_about {
            self.toast.show(
                "Local workspace data stays on this device. Microsoft 365 chat reads and plain-text sends are optional; other collaboration mutations are not enabled.",
            );
        }
    }

    fn draw_shortcuts_dialog(&mut self, ctx: &egui::Context) {
        if !self.shortcuts_open {
            return;
        }
        if take_escape(ctx) {
            self.shortcuts_open = false;
            return;
        }

        let mut close_requested = false;
        let shortcut_groups = shortcut_groups();
        let _ = modal::show(ctx, "keyboard_shortcuts", 450.0, |ui| {
            modal::heading(ui, "Keyboard shortcuts");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Navigate the local workspace without leaving the keyboard.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
            for (index, group) in shortcut_groups.iter().enumerate() {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(&group.title)
                        .font(theme::semibold(theme::font_size::CAPTION))
                        .color(theme::fg1()),
                );
                egui::Grid::new(("shortcut_grid", index))
                    .num_columns(2)
                    .spacing(egui::vec2(26.0, 7.0))
                    .show(ui, |ui| {
                        for (keys, action) in &group.rows {
                            ui.label(RichText::new(keys).color(theme::brand_fg()));
                            ui.label(RichText::new(action).color(theme::fg1()));
                            ui.end_row();
                        }
                    });
            }
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close_requested = true;
                }
            });
        });
        if close_requested {
            self.shortcuts_open = false;
        }
    }

    fn draw_sidebar(&mut self, ctx: &egui::Context) {
        if self.sidebar_collapsed {
            return;
        }
        egui::SidePanel::left("list")
            .exact_width(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(
                        if self.nav.section == Section::Chat || self.nav.section == Section::Teams {
                            theme::sidebar_tint()
                        } else {
                            theme::surface_subtle()
                        },
                    )
                    .inner_margin(egui::Margin::ZERO)
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
            )
            .show(ctx, |ui| {
                if self.nav.section == Section::Chat {
                    self.chat.draw_sidebar(ui, &mut self.toast);
                } else {
                    self.workspace
                        .draw_sidebar(ui, self.nav.section, &mut self.toast);
                }
            });
    }

    fn draw_surface(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::canvas())
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| {
                if self.nav.section == Section::Chat {
                    self.chat.draw_conversation(ui, &mut self.toast);
                } else {
                    self.workspace
                        .draw_page(ui, self.nav.section, &mut self.toast);
                }
            });
        self.start_requested_chat_call();
        if let Some(section) = self.workspace.take_navigation_request() {
            self.navigate_to(section);
        }
    }

    fn can_go_back(&self) -> bool {
        self.nav.can_go_back()
    }

    fn can_go_forward(&self) -> bool {
        self.nav.can_go_forward()
    }

    fn navigate_to(&mut self, section: Section) {
        self.search.dismiss();
        self.auth.dismiss_profile_menu();
        if self.nav.section == section {
            return;
        }
        self.dismiss_current_section_transients();
        self.nav.push(section);
    }

    fn toggle_profile_menu(&mut self) {
        self.auth.toggle_profile();
    }

    fn handle_rail_shortcut(&mut self, ctx: &egui::Context) {
        if !self.global_search_enabled() {
            return;
        }
        let section = ctx.input(|input| {
            input.modifiers.command.then(|| {
                RAIL_DESTINATIONS
                    .iter()
                    .find_map(|(_, _, shortcut)| {
                        input
                            .key_pressed(*shortcut)
                            .then(|| rail_shortcut_section(*shortcut))
                    })
                    .flatten()
            })
        });
        if let Some(section) = section.flatten() {
            self.navigate_to(section);
        }
    }

    /// Ctrl/Cmd+K belongs to the palette everywhere, including over a focused
    /// text field; it is consumed before any widget sees it. Shift-modified
    /// chords (a future link shortcut) do not match.
    fn handle_palette_shortcut(&mut self, ctx: &egui::Context) {
        let pressed = ctx.input_mut(|input| {
            // `consume_key` tolerates an extra Shift, so exclude it here.
            !input.modifiers.shift
                && !input.modifiers.alt
                && input.consume_key(egui::Modifiers::COMMAND, egui::Key::K)
        });
        if !pressed {
            return;
        }
        if self.palette.is_open() {
            self.palette.close();
        } else if self.global_search_enabled() {
            self.search.dismiss();
            self.palette.open();
        }
    }

    /// Ctrl/Cmd+, opens Settings (and closes it again). Like the palette chord
    /// it is consumed before any widget sees it; a comma typed with the
    /// command modifier held is never text, so a focused field loses nothing.
    fn handle_settings_shortcut(&mut self, ctx: &egui::Context) {
        let pressed = ctx.input_mut(|input| {
            !input.modifiers.shift
                && !input.modifiers.alt
                && input.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)
        });
        if !pressed {
            return;
        }
        if self.settings.is_open() {
            self.settings.close();
        } else if self.navigation_enabled() || self.palette.is_open() {
            self.open_settings(SettingsSection::Appearance);
        }
    }

    fn open_settings(&mut self, section: SettingsSection) {
        self.search.dismiss();
        self.palette.close();
        self.auth.dismiss_profile_menu();
        self.settings.open_section(section);
    }

    fn settings_view<'a>(
        &self,
        shortcuts: &'a [crate::ui::shortcuts::ShortcutGroup],
    ) -> SettingsView<'a> {
        SettingsView {
            theme: self.app_prefs.theme,
            notifications: self.chat.notifications_enabled(),
            show_chat_name: self.chat.show_chat_name_in_notifications(),
            os_notifications_available: self.chat.os_notifications_available(),
            os_notifications: self.chat.os_notifications_enabled(),
            read_receipts: self.chat.send_read_receipts(),
            read_receipts_offered: self.chat.read_receipts_offered(),
            microsoft365: Microsoft365View {
                connected: self.transport.is_microsoft365_active(),
                cloud: self.remote_auth.cloud_label(),
                account: self.transport.connected_account_name().map(str::to_owned),
                capabilities: self.chat.effective_remote_capabilities(),
            },
            shortcuts,
        }
    }

    /// Opens Settings for a request made by the account menu or the chat menu.
    fn open_requested_settings(&mut self) {
        let from_account_menu = self.auth.take_settings_request();
        if self.chat.take_settings_request() {
            self.open_settings(SettingsSection::Notifications);
        } else if from_account_menu {
            self.open_settings(SettingsSection::Appearance);
        }
    }

    fn draw_settings_dialog(&mut self, ctx: &egui::Context) {
        self.open_requested_settings();
        if !self.settings.is_open() {
            return;
        }
        let shortcuts = shortcut_groups();
        let view = self.settings_view(&shortcuts);
        for action in self.settings.draw(ctx, &view) {
            self.apply_settings_action(action, ctx);
        }
    }

    fn apply_settings_action(&mut self, action: SettingsAction, ctx: &egui::Context) {
        match action {
            SettingsAction::SetTheme(theme) => {
                if self.app_prefs.theme != theme {
                    self.app_prefs.theme = theme;
                    self.app_prefs_dirty = true;
                }
            }
            SettingsAction::SetNotifications(on) => self.chat.set_notifications_enabled(on),
            SettingsAction::SetShowChatName(on) => {
                self.chat.set_show_chat_name_in_notifications(on);
            }
            SettingsAction::SetOsNotifications(on) => self.chat.set_os_notifications_enabled(on),
            SettingsAction::SetReadReceipts(on) => self.chat.set_send_read_receipts(on),
            SettingsAction::DisconnectMicrosoft365 => self.disconnect_microsoft365(ctx),
        }
    }

    fn run_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Navigate(section) => self.navigate_to(section),
            PaletteAction::NewChat => {
                self.navigate_to(Section::Chat);
                self.chat.open_new_chat();
            }
            PaletteAction::NewMeeting => {
                self.navigate_to(Section::Calendar);
                self.workspace.open_new_meeting();
            }
            PaletteAction::ToggleSidebar => self.sidebar_collapsed = !self.sidebar_collapsed,
            PaletteAction::Refresh => self.refresh.signal(RefreshTrigger::Explicit),
            PaletteAction::ShowShortcuts => self.shortcuts_open = true,
            PaletteAction::OpenSettings => self.open_settings(SettingsSection::Appearance),
            PaletteAction::SignOut => self.auth.sign_out(),
        }
    }

    fn run_search_action(&mut self, action: SearchAction) {
        match action {
            SearchAction::RemoteSearch(text) => self.search.start_remote_search(&text),
            SearchAction::RemoteSearchMore => self.search.request_more_remote_results(),
            SearchAction::OpenRemoteHit(location) => self.open_remote_search_hit(location),
            SearchAction::Navigate(section) => self.navigate_to(section),
            SearchAction::OpenChat(target) => {
                self.chat.select_search_result(target);
                self.navigate_to(Section::Chat);
            }
            SearchAction::OpenWorkspace(target) => {
                let section = self.workspace.select_search_result(target);
                self.navigate_to(section);
            }
        }
    }

    /// Opens a Microsoft 365 search hit by its opaque target, or says why it
    /// cannot be shown. The overlay has already closed.
    fn open_remote_search_hit(&mut self, location: crate::integrations::RemoteSearchLocation) {
        use crate::integrations::RemoteSearchLocation;
        match location {
            RemoteSearchLocation::Chat { chat, message } => {
                if self.chat.open_remote_chat(&chat, message.as_ref()) {
                    self.navigate_to(Section::Chat);
                } else {
                    self.toast
                        .show("That chat is not in your current chat list.");
                }
            }
            RemoteSearchLocation::Channel { team, channel } => {
                if self
                    .workspace
                    .open_remote_channel(team, channel, &mut self.toast)
                {
                    self.navigate_to(Section::Teams);
                } else {
                    self.toast
                        .show("That channel is not in your current Teams list.");
                }
            }
        }
    }

    /// Hands the overlay's search request or cancellation to the transport.
    fn drive_remote_message_search(&mut self) {
        if self.search.take_remote_cancellation_requested() {
            self.transport.cancel_remote_message_search();
        }
        if let Some((query, page)) = self.search.take_remote_request() {
            self.transport.request_remote_message_search(query, page);
        }
    }

    fn start_requested_chat_call(&mut self) {
        let Some((participant, mode)) = self.chat.take_requested_call() else {
            return;
        };
        if let Some(participant) = self.workspace.start_chat_call(&participant, mode) {
            self.toast.show(format!(
                "Started a local {} call to {participant}",
                mode.label().to_lowercase()
            ));
        } else {
            self.toast
                .show_error("End the active local call or meeting before starting another.");
        }
    }

    /// A sign-out clears the on-device preview cache as well as transient
    /// session state, so drafts, chat history, calendar data, Activity, and
    /// Teams mutations cannot cross local profiles.
    fn reset_local_session(&mut self, ctx: &egui::Context) {
        self.remote_auth.cancel();
        self.clear_local_presence_store();
        self.clear_local_chat_store();
        self.clear_local_chat_prefs_store();
        self.clear_local_app_prefs_store();
        self.clear_local_calendar_store();
        self.clear_local_activity_store();
        self.clear_local_teams_store();
        self.clear_local_threads_store();
        self.clear_local_calls_store();
        self.clear_local_files_store();
        self.transport.reset();
        self.refresh.reset();
        self.read_receipt_intents.clear();
        self.upload_intents.clear();
        self.nav.reset();
        self.chat = ChatState::default();
        self.search = SearchState::default();
        self.palette = PaletteState::default();
        self.settings = SettingsState::default();
        self.app_prefs = app_prefs::AppPrefs::default();
        self.app_prefs_dirty = false;
        self.workspace = WorkspaceState::default();
        self.toast = ToastState::default();
        self.shortcuts_open = false;
        self.pending_local_storage_notice = None;
        self.close_blocked_by_save_failure = false;
        // egui keeps text-edit state (including the undo history) per widget
        // id, so one profile's typed text could be undone into the next.
        // Per-widget UI state is cleared; fonts, visuals and style live
        // outside this store and are kept.
        ctx.data_mut(|data| data.clear());
        ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }

    /// Persists every dirty store before the window closes, bypassing the
    /// save debounce. If a store cannot be saved the first close request is
    /// cancelled and the problem is shown; closing again quits anyway.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        let failures = self.flush_all_local_stores_now(ctx);
        if failures.is_empty() || self.close_blocked_by_save_failure {
            return;
        }
        self.close_blocked_by_save_failure = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.toast.show_error(
            "Some local data could not be saved. Close again to quit without saving it.",
        );
    }

    /// A persistent, non-dismissible strip while any local store cannot be
    /// saved (for example, when a snapshot has outgrown its size limit). It
    /// clears itself once a later save succeeds.
    fn draw_local_storage_warnings(&mut self, ctx: &egui::Context) {
        let warnings = self.local_storage_warnings();
        if warnings.is_empty() {
            // Every store recovered, so a later failure must block the next
            // close request again instead of quitting silently.
            self.close_blocked_by_save_failure = false;
            return;
        }
        egui::TopBottomPanel::bottom("local_storage_warnings")
            .frame(
                egui::Frame::new()
                    .fill(theme::surface_subtle())
                    .stroke(Stroke::new(1.0_f32, theme::danger()))
                    .inner_margin(egui::Margin::symmetric(
                        theme::space::LG as i8,
                        theme::space::SM as i8,
                    )),
            )
            .show(ctx, |ui| {
                for warning in &warnings {
                    ui.label(
                        RichText::new(warning)
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::danger()),
                    );
                }
            });
    }

    fn dismiss_current_section_transients(&mut self) {
        match self.nav.section {
            Section::Chat => self.chat.dismiss_contextual_overlays(),
            section => self.workspace.dismiss_section_transients(section),
        }
    }

    fn go_back(&mut self) {
        if !self.can_go_back() {
            return;
        }
        self.search.dismiss();
        self.auth.dismiss_profile_menu();
        self.dismiss_current_section_transients();
        self.nav.step_back();
    }

    fn go_forward(&mut self) {
        if !self.can_go_forward() {
            return;
        }
        self.search.dismiss();
        self.auth.dismiss_profile_menu();
        self.dismiss_current_section_transients();
        self.nav.step_forward();
    }
}

/// The shortcut list shown by the shortcuts dialog and by Settings.
fn shortcut_groups() -> Vec<crate::ui::shortcuts::ShortcutGroup> {
    let apps: Vec<(String, String)> = RAIL_DESTINATIONS
        .iter()
        .map(|(_, label, key)| (key.name().to_owned(), (*label).to_owned()))
        .collect();
    crate::ui::shortcuts::groups(&apps)
}

pub(crate) fn run() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(REFERENCE_WINDOW_SIZE)
        .with_min_inner_size([900.0, 600.0])
        .with_app_id("io.github.vakesz.teamspace");
    // The icon is a bundled constant; a decode failure only loses the icon.
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../../assets/app-icon/teamspace-256.png"))
    {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Teamspace",
        options,
        Box::new(|cc| {
            Ok(Box::new(TeamsApp::create(
                cc,
                TeamsApp::with_local_persistence(),
            )))
        }),
    )
}

#[cfg(test)]
mod preview_tests;
#[cfg(test)]
mod tests;
