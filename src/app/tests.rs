use super::{
    RAIL_DESTINATIONS, TeamsApp, launch_remote_browser_handoff_links,
    remote_channel_post_delivery_uncertain_toast, remote_channel_reply_delivery_uncertain_toast,
    remote_file_open_toast, remote_meeting_join_toast, shortcut_groups,
};
use crate::app::browser::BrowserOpenError;
use crate::app::local_stores::LocalStores;
use crate::app::navigation::Navigation;
use crate::app::persistence::LocalFeatureStore;
use crate::app::presence::{LocalPresence, LocalPresenceStore};
use crate::app::workspace_coordinator::WorkspaceState;
use crate::app::workspace_transport::WorkspaceTransportController;
use crate::features::activity::{LocalActivityStore, StoredActivity};
use crate::features::calendar::LocalCalendarStore;
use crate::features::calls::{CallMode, LocalCallsStore, StoredCalls};
use crate::features::chat::{ChatSearchTarget, ChatState, LocalChatStore};
use crate::features::files::{
    LocalFilesStore, RemoteFileItem, RemoteFileKind, RemoteFilePreview, RemoteFilePreviewContent,
    RemoteFilesDirectory, StoredFiles,
};
use crate::features::teams::{LocalTeamsStore, StoredTeams};
use crate::features::threads::{LocalThreadStore, ThreadId, ThreadStore};
use crate::integrations::{
    AccountIdentity, CommandDisposition, CommandReceipt, ConnectionStatus, ProviderBrowserUrl,
    ProviderResourceRef, RemoteCalendar, RemoteCalendarEvent, RemoteCalendarProjection,
    RemoteChannel, RemoteChannelFilesProjection, RemoteChannelMembership,
    RemoteChannelMessagesProjection, RemoteChannelPost, RemoteChannelRepliesProjection,
    RemoteChannelReply, RemoteChatConversation, RemoteChatKind, RemoteChatMessage,
    RemoteChatMessagesProjection, RemoteChatProjection, RemoteDriveItem, RemoteDriveItemKind,
    RemoteFilePreviewPayload, RemoteFilePreviewProjection, RemoteFilesProjection, RemoteTeam,
    RemoteTeamsProjection, SyncInterest, TransportError, WorkspaceCommand, WorkspaceDelta,
    WorkspaceTransport,
};
use crate::shared::model::Section;
use crate::testing::{render_headlessly, with_escape_press};
use crate::ui::theme::ThemePreference;
use crate::ui::toast::ToastState;
use eframe::egui;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMPORARY_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn channel_post_uncertainty_toast_distinguishes_live_and_lost_transport() {
    let error = TransportError::Unavailable("delivery response was lost".into());

    let live = remote_channel_post_delivery_uncertain_toast(true, &error);
    assert!(live.contains("bounded channel refresh is attempted"));
    assert!(live.contains("retained draft"));

    let lost = remote_channel_post_delivery_uncertain_toast(false, &error);
    assert!(lost.contains("connection is unavailable"));
    assert!(lost.contains("no channel refresh is possible"));
    assert!(lost.contains("reconnect and inspect the channel"));
    assert!(!lost.contains("retained draft"));
    assert!(!lost.contains("refresh is attempted"));
}

#[test]
fn channel_reply_uncertainty_toast_distinguishes_live_and_lost_transport() {
    let error = TransportError::Unavailable("delivery response was lost".into());

    let live = remote_channel_reply_delivery_uncertain_toast(true, &error);
    assert!(live.contains("bounded reply refresh is attempted"));
    assert!(live.contains("retained draft"));

    let lost = remote_channel_reply_delivery_uncertain_toast(false, &error);
    assert!(lost.contains("connection is unavailable"));
    assert!(lost.contains("no reply refresh is possible"));
    assert!(lost.contains("reconnect and inspect the reply thread"));
    assert!(!lost.contains("retained draft"));
    assert!(!lost.contains("refresh is attempted"));
}

#[test]
fn remote_meeting_join_drain_is_injectable_and_never_echoes_the_link() {
    let join_link = "https://teams.microsoft.com/l/meetup-join/opaque-example";
    let mut launched: Vec<String> = Vec::new();

    let outcomes = launch_remote_browser_handoff_links(vec![join_link.to_owned()], |value| {
        launched.push(value.to_owned());
        Ok(())
    });

    assert_eq!(launched, vec![join_link.to_owned()]);
    assert_eq!(outcomes, [Ok(())]);
    let success = remote_meeting_join_toast(outcomes[0]);
    assert!(success.contains("Opening the Microsoft 365 meeting"));
    assert!(!success.contains(join_link));

    let failure = remote_meeting_join_toast(Err(BrowserOpenError::Unavailable));
    assert!(failure.contains("could not be opened"));
    assert!(!failure.contains(join_link));
}

#[test]
fn remote_file_open_drain_is_injectable_and_never_echoes_the_link() {
    let item_url = "https://contoso.sharepoint.com/:x:/r/personal/example/opaque-item";
    let mut launched: Vec<String> = Vec::new();

    let outcomes = launch_remote_browser_handoff_links(
        vec![ProviderBrowserUrl::for_test(item_url)],
        |value| {
            launched.push(value.to_owned());
            Ok(())
        },
    );

    assert_eq!(launched, vec![item_url.to_owned()]);
    assert_eq!(outcomes, [Ok(())]);
    let success = remote_file_open_toast(outcomes[0]);
    assert!(success.contains("Opening the Microsoft 365 item"));
    assert!(!success.contains(item_url));

    let failure = remote_file_open_toast(Err(BrowserOpenError::Unavailable));
    assert!(failure.contains("could not be opened"));
    assert!(!failure.contains(item_url));
}

#[test]
fn rich_link_clicks_are_revalidated_at_the_browser_boundary() {
    // The production launcher refuses anything but a strict https URL before
    // it can reach a host process, whatever the feature queued.
    let outcomes = launch_remote_browser_handoff_links(
        vec![
            "javascript:alert(1)",
            "http://example.com",
            "https://user:pw@example.com",
            "file:///etc/passwd",
        ],
        crate::app::browser::open_https_url,
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| *outcome == Err(BrowserOpenError::InvalidHttpsDestination))
    );
    let success = crate::app::remote_chat_link_toast(Ok(()));
    assert!(success.is_ok());
    let failure = crate::app::remote_chat_link_toast(Err(BrowserOpenError::Unavailable));
    assert!(failure.is_err_and(|message| !message.contains("://")));
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let sequence = TEMPORARY_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "teamspace-app-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("test directory must be creatable");
        Self(path)
    }

    fn chat_path(&self) -> PathBuf {
        self.0.join("profile").join("chat-v1.json")
    }

    fn presence_path(&self) -> PathBuf {
        self.0.join("profile").join("presence-v1.json")
    }

    fn calendar_path(&self) -> PathBuf {
        self.0.join("profile").join("calendar-v1.json")
    }

    fn activity_path(&self) -> PathBuf {
        self.0.join("profile").join("activity-v1.json")
    }

    fn teams_path(&self) -> PathBuf {
        self.0.join("profile").join("teams-v1.json")
    }

    fn threads_path(&self) -> PathBuf {
        self.0.join("profile").join("threads-v1.json")
    }

    fn calls_path(&self) -> PathBuf {
        self.0.join("profile").join("calls-v1.json")
    }

    fn files_path(&self) -> PathBuf {
        self.0.join("profile").join("files-v1.json")
    }

    fn local_upload_path(&self) -> PathBuf {
        let path = self.0.join("uploads").join("retained.txt");
        fs::create_dir_all(path.parent().expect("upload parent must exist"))
            .expect("upload parent must be creatable");
        fs::write(&path, b"local Files lifecycle fixture")
            .expect("local upload fixture must write");
        fs::canonicalize(path).expect("local upload fixture must canonicalize")
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn press_rail_shortcut(app: &mut TeamsApp, ctx: &egui::Context, key: egui::Key) {
    let modifiers = egui::Modifiers {
        command: true,
        mac_cmd: true,
        ..Default::default()
    };
    ctx.begin_pass(egui::RawInput {
        modifiers,
        events: vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }],
        ..Default::default()
    });
    app.handle_rail_shortcut(ctx);
    let _ = ctx.end_pass();
}

fn press_chord(app: &mut TeamsApp, ctx: &egui::Context, key: egui::Key, shift: bool) {
    let modifiers = egui::Modifiers {
        command: true,
        mac_cmd: true,
        shift,
        ..Default::default()
    };
    ctx.begin_pass(egui::RawInput {
        modifiers,
        events: vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }],
        ..Default::default()
    });
    app.handle_palette_shortcut(ctx);
    let _ = ctx.end_pass();
}

#[test]
fn command_k_toggles_the_palette_and_shift_variants_do_not() {
    let ctx = egui::Context::default();
    let mut app = TeamsApp::default();

    press_chord(&mut app, &ctx, egui::Key::K, true);
    assert!(!app.palette.is_open());

    press_chord(&mut app, &ctx, egui::Key::K, false);
    assert!(app.palette.is_open());
    // Open palette blocks rail navigation, and the chord closes it again.
    assert!(!app.navigation_enabled());
    press_chord(&mut app, &ctx, egui::Key::K, false);
    assert!(!app.palette.is_open());

    app.shortcuts_open = true;
    press_chord(&mut app, &ctx, egui::Key::K, false);
    assert!(!app.palette.is_open());
}

#[test]
fn palette_commands_navigate_and_open_dialogs() {
    use crate::features::palette::PaletteAction;
    let mut app = TeamsApp::default();
    app.run_palette_action(PaletteAction::Navigate(Section::Files));
    assert_eq!(app.nav.section, Section::Files);
    app.run_palette_action(PaletteAction::NewMeeting);
    assert_eq!(app.nav.section, Section::Calendar);
    app.run_palette_action(PaletteAction::NewChat);
    assert_eq!(app.nav.section, Section::Chat);
    app.run_palette_action(PaletteAction::ToggleSidebar);
    assert!(app.sidebar_collapsed);
    app.run_palette_action(PaletteAction::ShowShortcuts);
    assert!(app.shortcuts_open);
    app.run_palette_action(PaletteAction::OpenSettings);
    assert!(app.settings.is_open());
}

fn press_comma(app: &mut TeamsApp, ctx: &egui::Context, command: bool, shift: bool) {
    let modifiers = egui::Modifiers {
        command,
        mac_cmd: command,
        shift,
        ..Default::default()
    };
    ctx.begin_pass(egui::RawInput {
        modifiers,
        events: vec![egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }],
        ..Default::default()
    });
    app.handle_settings_shortcut(ctx);
    let _ = ctx.end_pass();
}

#[test]
fn command_comma_toggles_settings_and_plain_comma_does_not() {
    let ctx = egui::Context::default();
    let mut app = TeamsApp::default();

    press_comma(&mut app, &ctx, false, false);
    assert!(!app.settings.is_open());
    press_comma(&mut app, &ctx, true, true);
    assert!(!app.settings.is_open());

    press_comma(&mut app, &ctx, true, false);
    assert!(app.settings.is_open());
    assert!(!app.navigation_enabled());
    press_comma(&mut app, &ctx, true, false);
    assert!(!app.settings.is_open());

    // Another dialog blocks it; the palette is replaced by it.
    app.shortcuts_open = true;
    press_comma(&mut app, &ctx, true, false);
    assert!(!app.settings.is_open());
    app.shortcuts_open = false;
    app.palette.open();
    press_comma(&mut app, &ctx, true, false);
    assert!(app.settings.is_open());
    assert!(!app.palette.is_open());
}

#[test]
fn account_menu_and_chat_menu_requests_open_the_right_settings_section() {
    use crate::features::settings::SettingsSection;
    let mut app = TeamsApp::default();

    app.auth.request_settings_for_test();
    app.open_requested_settings();
    assert!(app.settings.is_open());
    assert_eq!(app.settings.section(), SettingsSection::Appearance);

    app.settings.close();
    app.chat.request_settings_for_test();
    app.open_requested_settings();
    assert!(app.settings.is_open());
    assert_eq!(app.settings.section(), SettingsSection::Notifications);
}

#[test]
fn settings_actions_change_the_owning_preferences() {
    use crate::features::settings::SettingsAction;
    let ctx = egui::Context::default();
    let mut app = TeamsApp::default();

    app.apply_settings_action(SettingsAction::SetTheme(ThemePreference::Dark), &ctx);
    assert_eq!(app.app_prefs.theme, ThemePreference::Dark);
    assert!(app.app_prefs_dirty);

    app.apply_settings_action(SettingsAction::SetNotifications(false), &ctx);
    assert!(!app.chat.notifications_enabled());
    app.apply_settings_action(SettingsAction::SetShowChatName(true), &ctx);
    assert!(app.chat.show_chat_name_in_notifications());
    app.apply_settings_action(SettingsAction::SetReadReceipts(true), &ctx);
    assert!(app.chat.send_read_receipts());
    assert!(app.chat.is_prefs_dirty());
}

#[test]
fn the_theme_choice_persists_per_profile_and_is_cleared_on_reset() {
    use crate::app::app_prefs::{AppPrefs, LocalAppPrefsStore};
    let directory = TemporaryDirectory::new();
    let store = LocalAppPrefsStore::at(directory.chat_path().with_file_name("app-prefs-v1.json"));
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            app_prefs_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    app.app_prefs.theme = ThemePreference::Dark;
    app.app_prefs_dirty = true;
    app.stores.app_prefs_store.make_save_due();
    app.flush_local_app_prefs(&egui::Context::default());
    assert_eq!(
        store.load().expect("app prefs must load"),
        Some(AppPrefs {
            theme: ThemePreference::Dark
        })
    );
    assert!(!app.app_prefs_dirty);

    app.reset_local_session(&egui::Context::default());
    assert_eq!(store.load().expect("cleared prefs must load"), None);
    assert_eq!(app.app_prefs, AppPrefs::default());
}

#[test]
fn the_shortcut_list_matches_the_rail_destinations_and_includes_settings() {
    let groups = shortcut_groups();
    let apps = &groups[0];
    assert_eq!(apps.rows.len(), RAIL_DESTINATIONS.len());
    for ((_, label, key), (keys, action)) in RAIL_DESTINATIONS.iter().zip(&apps.rows) {
        assert_eq!(keys, &crate::ui::shortcuts::primary_chord(key.name()));
        assert_eq!(action, label);
    }
    assert!(
        groups
            .iter()
            .flat_map(|group| &group.rows)
            .any(|(keys, action)| action == "Open settings"
                && keys == &crate::ui::shortcuts::primary_chord(","))
    );
}

fn render_at_minimum_window_size(app: &mut TeamsApp) {
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    ctx.begin_pass(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 600.0),
        )),
        ..Default::default()
    });
    let search_enabled = app.global_search_enabled();
    let _ = app.draw_topbar(&ctx, search_enabled);
    app.draw_rail(&ctx);
    app.auth.render_profile_menu(&ctx);
    app.draw_sidebar(&ctx);
    app.draw_surface(&ctx);
    app.workspace.draw_transient_overlays(&ctx, &mut app.toast);
    app.draw_shortcuts_dialog(&ctx);
    app.toast.draw(&ctx);
    let _ = ctx.end_pass();
}

struct RemoteProjectionTransport {
    projection: RemoteChatProjection,
    chat_messages_projection: RemoteChatMessagesProjection,
    teams_projection: RemoteTeamsProjection,
    channel_messages_projection: RemoteChannelMessagesProjection,
    channel_files_projection: RemoteChannelFilesProjection,
    channel_replies_projection: RemoteChannelRepliesProjection,
    calendar_projection: RemoteCalendarProjection,
    files_projection: RemoteFilesProjection,
}

impl RemoteProjectionTransport {
    fn identity() -> AccountIdentity {
        AccountIdentity {
            account_id: "remote-account".into(),
            display_name: "Remote Account".into(),
            email: Some("remote@example.test".into()),
            tenant_id: None,
        }
    }
}

impl WorkspaceTransport for RemoteProjectionTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Connected(Self::identity())
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        match interest {
            SyncInterest::Identity => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Identity],
                identity: Some(Self::identity()),
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                identity: None,
                remote_chats: Some(self.projection.clone()),
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::ChatMessages { chat } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChatMessages { chat }],
                identity: None,
                remote_chats: None,
                remote_chat_messages: Some(self.chat_messages_projection.clone()),
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Teams => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: Some(self.teams_projection.clone()),
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::ChannelMessages { team, channel } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelMessages { team, channel }],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: Some(self.channel_messages_projection.clone()),
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::ChannelFiles { team, channel } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelFiles { team, channel }],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: Some(self.channel_files_projection.clone()),
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelReplies {
                    team,
                    channel,
                    post,
                }],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: Some(self.channel_replies_projection.clone()),
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Calendar => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Calendar],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: Some(self.calendar_projection.clone()),
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Files => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Files],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: Some(self.files_projection.clone()),
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::OneDriveFilePreview { item } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::OneDriveFilePreview { item }],
                remote_file_preview: Some(RemoteFilePreviewProjection::for_test(
                    "remote-drive-item-id",
                    RemoteFilePreviewPayload::Text("A session-only remote note.".into()),
                )),
                ..WorkspaceDelta::default()
            }),
            _ => Err(TransportError::Unsupported(
                "not configured by this app integration test".into(),
            )),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Ok(CommandReceipt {
            command_id: "test:local".into(),
            disposition: CommandDisposition::StagedLocally,
            upload: None,
        })
    }
}

fn remote_projection_for_app_test() -> RemoteChatProjection {
    RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
        "19:remote-chat@thread.v2",
        RemoteChatKind::OneOnOne,
        "Ada Lovelace",
        "Remote design review",
        "AL",
        true,
        vec![RemoteChatMessage::for_test(
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "This remote message must remain ephemeral.",
            false,
            false,
        )],
    )])
}

fn remote_chat_messages_projection_for_app_test() -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(
        "19:remote-chat@thread.v2",
        vec![RemoteChatMessage::for_test(
            "Ada Lovelace",
            "2026-11-06T10:31:00Z",
            "This selected remote history must remain ephemeral.",
            false,
            false,
        )],
    )
}

fn remote_teams_projection_for_app_test() -> RemoteTeamsProjection {
    RemoteTeamsProjection::for_test(vec![RemoteTeam::for_test(
        "remote-team-id",
        "Remote Engineering",
        "A tenant-owned engineering workspace.",
        vec![RemoteChannel::for_test(
            "remote-channel-id",
            "Planning",
            "Delivery planning and status updates.",
            RemoteChannelMembership::Private,
            false,
        )],
    )])
}

fn remote_channel_messages_projection_for_app_test() -> RemoteChannelMessagesProjection {
    RemoteChannelMessagesProjection::for_test(
        "remote-team-id",
        "remote-channel-id",
        vec![RemoteChannelPost::for_test(
            "remote-channel-post-id",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote delivery update",
            "This root post must remain session-only.",
            false,
            false,
        )],
    )
}

fn remote_channel_replies_projection_for_app_test() -> RemoteChannelRepliesProjection {
    RemoteChannelRepliesProjection::for_test(
        "remote-team-id",
        "remote-channel-id",
        "remote-channel-post-id",
        vec![RemoteChannelReply::for_test(
            "remote-channel-reply-id",
            "Grace Hopper",
            "2026-11-06T10:35:00Z",
            "The reply must remain session-only too.",
            false,
            false,
        )],
    )
}

fn remote_channel_files_projection_for_app_test() -> RemoteChannelFilesProjection {
    RemoteChannelFilesProjection::for_test(
        "remote-team-id",
        "remote-channel-id",
        vec![RemoteDriveItem::for_test(
            "remote-channel-drive-item-id",
            "Channel delivery plan.docx",
            "2026-11-06T10:40:00Z",
            Some(8_192),
            RemoteDriveItemKind::File,
        )],
    )
}

fn remote_calendar_projection_for_app_test() -> RemoteCalendarProjection {
    RemoteCalendarProjection::for_test(Some(RemoteCalendar::for_test(
        "remote-calendar-id",
        "Remote engineering calendar",
        true,
        vec![RemoteCalendarEvent::for_test(
            "remote-calendar-event-id",
            "Remote design review",
            "2026-11-06T10:30:00.0000000",
            "2026-11-06T11:00:00.0000000",
            "UTC",
            "Design studio",
            "Ada Lovelace",
            true,
            Some("https://teams.microsoft.com/l/meetup-join/example".into()),
            false,
        )],
    )))
}

fn remote_files_projection_for_app_test() -> RemoteFilesProjection {
    RemoteFilesProjection::for_test(vec![RemoteDriveItem::for_test(
        "remote-drive-item-id",
        "Delivery plan.docx",
        "2026-11-06T10:30:00Z",
        Some(4_096),
        RemoteDriveItemKind::File,
    )])
}

fn remote_file_preview_target_for_app_test() -> crate::integrations::ProviderResourceRef {
    RemoteFilePreviewProjection::for_test(
        "remote-drive-item-id",
        RemoteFilePreviewPayload::Text("A session-only remote note.".into()),
    )
    .target()
}

#[test]
fn app_applies_and_clears_remote_chat_deltas_without_persisting_them() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.chat.stored();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: remote_projection_for_app_test(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: RemoteTeamsProjection::default(),
            channel_messages_projection: RemoteChannelMessagesProjection::for_test(
                "remote-team-id",
                "remote-channel-id",
                Vec::new(),
            ),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app
            .chat
            .search_results("ada")
            .iter()
            .any(|result| matches!(result.target, ChatSearchTarget::Remote(_)))
        {
            break;
        }
        std::thread::yield_now();
    }

    assert!(
        app.chat
            .search_results("ada")
            .iter()
            .any(|result| { matches!(result.target, ChatSearchTarget::Remote(_)) })
    );
    assert_eq!(app.chat.stored(), local_snapshot);
    assert!(!app.chat.is_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(
        !app.chat
            .search_results("ada")
            .iter()
            .any(|result| { matches!(result.target, ChatSearchTarget::Remote(_)) })
    );
    assert_eq!(app.chat.stored(), local_snapshot);
    assert!(!app.chat.is_persistence_dirty());
}

#[test]
fn app_loads_selected_remote_chat_history_through_the_transport_without_persisting_it() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.chat.stored();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: remote_projection_for_app_test(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: RemoteTeamsProjection::default(),
            channel_messages_projection: RemoteChannelMessagesProjection::for_test(
                "remote-team-id",
                "remote-channel-id",
                Vec::new(),
            ),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app
            .chat
            .search_results("ada")
            .iter()
            .any(|result| matches!(result.target, ChatSearchTarget::Remote(_)))
        {
            break;
        }
        std::thread::yield_now();
    }
    app.chat.select_remote_conversation(0);
    app.flush_workspace_transport(&ctx);

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app
            .chat
            .search_results("selected remote history")
            .iter()
            .any(|result| matches!(result.target, ChatSearchTarget::Remote(_)))
        {
            break;
        }
        std::thread::yield_now();
    }

    assert!(
        app.chat
            .search_results("selected remote history")
            .iter()
            .any(|result| matches!(result.target, ChatSearchTarget::Remote(_))),
        "the selected history must cross the target-bound transport seam"
    );
    assert_eq!(app.chat.stored(), local_snapshot);
    assert!(!app.chat.is_persistence_dirty());
}

#[test]
fn app_applies_and_clears_remote_teams_deltas_without_persisting_them() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.teams_snapshot();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: remote_teams_projection_for_app_test(),
            channel_messages_projection: remote_channel_messages_projection_for_app_test(),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().has_remote_directory() {
            break;
        }
        std::thread::yield_now();
    }

    assert!(app.workspace.teams().has_remote_directory());
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.teams().has_remote_directory());
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());
}

#[test]
fn app_loads_selected_remote_channel_posts_through_the_transport_and_keeps_them_ephemeral() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.teams_snapshot();
    let channel_messages_projection = remote_channel_messages_projection_for_app_test();
    let team = channel_messages_projection.team();
    let channel = channel_messages_projection.channel();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: remote_teams_projection_for_app_test(),
            channel_messages_projection,
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().has_remote_directory() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(app.workspace.teams().has_remote_directory());

    assert!(app.workspace.select_remote_channel_for_test(team, channel));
    app.flush_workspace_transport(&ctx);

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().remote_channel_message_count() == 1 {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(app.workspace.teams().remote_channel_message_count(), 1);
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.teams().has_remote_directory());
    assert_eq!(app.workspace.teams().remote_channel_message_count(), 0);
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());
}

#[test]
fn app_loads_and_clears_selected_remote_channel_files_without_persisting_them() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.teams_snapshot();
    let channel_messages_projection = remote_channel_messages_projection_for_app_test();
    let channel_files_projection = remote_channel_files_projection_for_app_test();
    let team = channel_files_projection.team();
    let channel = channel_files_projection.channel();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: remote_teams_projection_for_app_test(),
            channel_messages_projection,
            channel_files_projection,
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().has_remote_directory() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(app.workspace.teams().has_remote_directory());

    assert!(
        app.workspace
            .select_remote_channel_for_test(team.clone(), channel.clone())
    );
    assert!(
        app.workspace
            .select_remote_channel_files_for_test(team, channel)
    );
    app.flush_workspace_transport(&ctx);

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().remote_channel_file_count() == 1 {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(app.workspace.teams().remote_channel_file_count(), 1);
    assert!(!app.workspace.teams().is_loading_remote_channel_files());
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.teams().has_remote_directory());
    assert_eq!(app.workspace.teams().remote_channel_file_count(), 0);
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());
}

#[test]
fn app_loads_and_clears_selected_remote_channel_replies_without_persisting_them() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.teams_snapshot();
    let channel_messages_projection = remote_channel_messages_projection_for_app_test();
    let channel_replies_projection = remote_channel_replies_projection_for_app_test();
    let team = channel_messages_projection.team();
    let channel = channel_messages_projection.channel();
    let post = channel_replies_projection.post();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: remote_teams_projection_for_app_test(),
            channel_messages_projection,
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection,
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().has_remote_directory() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(app.workspace.teams().has_remote_directory());

    assert!(
        app.workspace
            .select_remote_channel_for_test(team.clone(), channel.clone())
    );
    app.flush_workspace_transport(&ctx);
    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().remote_channel_message_count() == 1 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(app.workspace.teams().remote_channel_message_count(), 1);

    assert!(
        app.workspace
            .select_remote_channel_post_for_test(team, channel, post)
    );
    app.flush_workspace_transport(&ctx);
    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.teams().remote_channel_reply_count() == 1 {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(app.workspace.teams().remote_channel_reply_count(), 1);
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.teams().has_remote_directory());
    assert_eq!(app.workspace.teams().remote_channel_message_count(), 0);
    assert_eq!(app.workspace.teams().remote_channel_reply_count(), 0);
    assert_eq!(app.workspace.teams_snapshot(), local_snapshot);
    assert!(!app.workspace.is_teams_persistence_dirty());
}

#[test]
fn app_applies_and_clears_remote_calendar_deltas_without_persisting_them() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.calendar_snapshot();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: RemoteTeamsProjection::default(),
            channel_messages_projection: RemoteChannelMessagesProjection::for_test(
                "remote-team-id",
                "remote-channel-id",
                Vec::new(),
            ),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: remote_calendar_projection_for_app_test(),
            files_projection: RemoteFilesProjection::default(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.calendar().has_remote_directory() {
            break;
        }
        std::thread::yield_now();
    }

    assert!(app.workspace.calendar().has_remote_directory());
    assert_eq!(app.workspace.calendar_snapshot(), local_snapshot);
    assert!(!app.workspace.is_calendar_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.calendar().has_remote_directory());
    assert_eq!(app.workspace.calendar_snapshot(), local_snapshot);
    assert!(!app.workspace.is_calendar_persistence_dirty());
}

#[test]
fn app_applies_and_clears_remote_onedrive_metadata_without_persisting_it() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.files_snapshot();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: RemoteTeamsProjection::default(),
            channel_messages_projection: RemoteChannelMessagesProjection::for_test(
                "remote-team-id",
                "remote-channel-id",
                Vec::new(),
            ),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: remote_files_projection_for_app_test(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.files_mut().has_remote_files_directory() {
            break;
        }
        std::thread::yield_now();
    }

    assert!(app.workspace.files_mut().has_remote_files_directory());
    assert_eq!(app.workspace.files_mut().remote_file_count(), 1);
    assert_eq!(app.workspace.files_snapshot(), local_snapshot);
    assert!(!app.workspace.is_files_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.transport.is_microsoft365_active());
    assert!(!app.workspace.files_mut().has_remote_files_directory());
    assert_eq!(app.workspace.files_mut().remote_file_count(), 0);
    assert_eq!(app.workspace.files_snapshot(), local_snapshot);
    assert!(!app.workspace.is_files_persistence_dirty());
}

#[test]
fn app_loads_and_clears_a_selected_remote_onedrive_preview_without_persisting_it() {
    let mut app = TeamsApp::default();
    let local_snapshot = app.workspace.files_snapshot();
    app.transport =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteProjectionTransport {
            projection: RemoteChatProjection::default(),
            chat_messages_projection: remote_chat_messages_projection_for_app_test(),
            teams_projection: RemoteTeamsProjection::default(),
            channel_messages_projection: RemoteChannelMessagesProjection::for_test(
                "remote-team-id",
                "remote-channel-id",
                Vec::new(),
            ),
            channel_files_projection: remote_channel_files_projection_for_app_test(),
            channel_replies_projection: remote_channel_replies_projection_for_app_test(),
            calendar_projection: RemoteCalendarProjection::default(),
            files_projection: remote_files_projection_for_app_test(),
        }));
    let ctx = egui::Context::default();

    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app.workspace.files_mut().has_remote_files_directory() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(app.workspace.files_mut().has_remote_files_directory());

    assert!(
        app.workspace
            .files_mut()
            .request_remote_file_preview_for_test(remote_file_preview_target_for_app_test())
    );
    app.flush_workspace_transport(&ctx);
    for _ in 0..10_000 {
        app.synchronize_workspace_transport(&ctx);
        if app
            .workspace
            .files_mut()
            .remote_file_preview_is_ready_for_test()
        {
            break;
        }
        std::thread::yield_now();
    }

    assert!(
        app.workspace
            .files_mut()
            .remote_file_preview_is_ready_for_test()
    );
    assert_eq!(app.workspace.files_snapshot(), local_snapshot);
    assert!(!app.workspace.is_files_persistence_dirty());

    app.disconnect_microsoft365(&ctx);

    assert!(!app.workspace.files_mut().has_remote_files_directory());
    assert!(
        !app.workspace
            .files_mut()
            .remote_file_preview_is_ready_for_test()
    );
    assert_eq!(app.workspace.files_snapshot(), local_snapshot);
    assert!(!app.workspace.is_files_persistence_dirty());
}

#[test]
fn disconnect_immediately_evicts_a_dropped_remote_onedrive_image_from_egui() {
    const ONE_PIXEL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207, 192, 240, 31,
        0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let cache_key = 0x9f04_u64;
    let image_uri = format!("bytes://teamspace/microsoft365-preview/{cache_key:016x}");
    let ctx = crate::ui::testing::themed_context();
    // Simulate the loader state already registered by the image widget. The
    // feature test exercises that widget; this app test covers the direct
    // disconnect boundary, which no longer draws the Files surface.
    ctx.include_bytes(image_uri.clone(), ONE_PIXEL_PNG.to_vec());
    assert!(ctx.try_load_bytes(&image_uri).is_ok());

    let target = ProviderResourceRef::local_preview("app-disconnect-image-eviction");
    let mut app = TeamsApp::default();
    app.workspace
        .files_mut()
        .replace_remote_files_directory(RemoteFilesDirectory::microsoft365(vec![
            RemoteFileItem::microsoft365(
                target.clone(),
                "Preview.png".into(),
                "2026-11-06T10:30:00Z".into(),
                Some(67),
                RemoteFileKind::File,
                "PNG image".into(),
                None,
            ),
        ]));
    assert!(
        app.workspace
            .files_mut()
            .request_remote_file_preview_for_test(target.clone())
    );
    assert!(
        app.workspace
            .files_mut()
            .apply_remote_file_preview(RemoteFilePreview::microsoft365(
                target,
                RemoteFilePreviewContent::Image {
                    bytes: std::sync::Arc::from(ONE_PIXEL_PNG),
                    cache_key,
                },
            ))
    );

    app.disconnect_microsoft365(&ctx);

    assert!(ctx.try_load_bytes(&image_uri).is_err());
}

#[test]
fn section_navigation_tracks_back_and_forward_history() {
    let mut app = TeamsApp::default();
    app.navigate_to(Section::Activity);
    app.navigate_to(Section::Calendar);

    assert!(app.can_go_back());
    assert!(!app.can_go_forward());
    app.go_back();
    assert_eq!(app.nav.section, Section::Activity);
    assert!(app.can_go_forward());

    app.go_forward();
    assert_eq!(app.nav.section, Section::Calendar);
    app.go_back();
    app.navigate_to(Section::Calls);
    assert_eq!(app.nav.section, Section::Calls);
    assert!(!app.can_go_forward());
}

#[test]
fn navigation_and_history_dismiss_the_global_search_overlay() {
    let mut app = TeamsApp::default();
    app.search.query_mut().push_str("calendar");
    app.search.open();
    app.navigate_to(Section::Chat);
    assert!(!app.search.is_open());
    assert!(app.search.query_mut().is_empty());

    app.navigate_to(Section::Teams);
    app.search.query_mut().push_str("facilities");
    app.search.open();
    app.go_back();
    assert_eq!(app.nav.section, Section::Chat);
    assert!(!app.search.is_open());

    app.search.query_mut().push_str("teams");
    app.search.open();
    app.go_forward();
    assert_eq!(app.nav.section, Section::Teams);
    assert!(!app.search.is_open());
}

#[test]
fn rail_shortcuts_match_the_reference_order() {
    assert_eq!(
        super::rail_shortcut_section(egui::Key::Num1),
        Some(Section::Activity)
    );
    assert_eq!(
        super::rail_shortcut_section(egui::Key::Num3),
        Some(Section::Teams)
    );
    assert_eq!(
        super::rail_shortcut_section(egui::Key::Num6),
        Some(Section::Files)
    );
    assert_eq!(super::rail_shortcut_section(egui::Key::Num0), None);
}

#[test]
fn topbar_search_keeps_a_bounded_account_region_at_minimum_width() {
    assert_eq!(super::topbar_search_width(739.0), 451.0);
    assert_eq!(super::topbar_search_width(2_000.0), 640.0);
}

#[test]
fn global_search_yields_to_shell_modal_controls() {
    let mut app = TeamsApp::default();

    assert!(app.global_search_enabled());
    app.shortcuts_open = true;
    assert!(!app.global_search_enabled());

    app.shortcuts_open = false;
    app.auth.toggle_profile();
    assert!(!app.global_search_enabled());
}

#[test]
fn contextual_dialogs_block_shell_navigation_but_the_account_popover_does_not() {
    assert!(TeamsApp::default().navigation_enabled());
    assert!(
        !TeamsApp {
            shortcuts_open: true,
            ..Default::default()
        }
        .navigation_enabled()
    );

    let mut app = TeamsApp::default();
    app.auth.toggle_profile();
    assert!(app.navigation_enabled());
    assert!(!app.global_search_enabled());
}

#[test]
fn command_rail_shortcuts_navigate_and_preserve_section_history() {
    let ctx = egui::Context::default();
    let mut app = TeamsApp::default();

    press_rail_shortcut(&mut app, &ctx, egui::Key::Num3);
    assert_eq!(app.nav.section, Section::Teams);
    assert_eq!(app.nav.history, [Section::Chat, Section::Teams]);

    press_rail_shortcut(&mut app, &ctx, egui::Key::Num5);
    assert_eq!(app.nav.section, Section::Calls);
    assert_eq!(
        app.nav.history,
        [Section::Chat, Section::Teams, Section::Calls]
    );
}

#[test]
fn rail_shortcuts_yield_to_shell_modal_controls() {
    let ctx = egui::Context::default();
    let mut app = TeamsApp {
        shortcuts_open: true,
        ..Default::default()
    };

    press_rail_shortcut(&mut app, &ctx, egui::Key::Num3);
    assert_eq!(app.nav.section, Section::Chat);

    app.shortcuts_open = false;
    app.auth.toggle_profile();
    press_rail_shortcut(&mut app, &ctx, egui::Key::Num3);
    assert_eq!(app.nav.section, Section::Chat);
    assert!(app.auth.is_profile_open());
}

#[test]
fn rail_account_control_uses_the_existing_profile_menu() {
    let mut app = TeamsApp::default();

    assert!(!app.auth.is_profile_open());
    app.toggle_profile_menu();
    assert!(app.auth.is_profile_open());
    app.toggle_profile_menu();
    assert!(!app.auth.is_profile_open());
}

#[test]
fn navigation_and_history_dismiss_the_account_popover() {
    let mut app = TeamsApp::default();

    app.auth.toggle_profile();
    app.navigate_to(Section::Chat);
    assert!(!app.auth.is_profile_open());

    app.navigate_to(Section::Teams);
    app.auth.toggle_profile();
    app.go_back();
    assert!(!app.auth.is_profile_open());

    app.auth.toggle_profile();
    app.go_forward();
    assert!(!app.auth.is_profile_open());
}

#[test]
fn shortcuts_dialog_is_escape_dismissible_without_changing_navigation() {
    let mut app = TeamsApp {
        shortcuts_open: true,
        ..Default::default()
    };
    let section = app.nav.section;

    with_escape_press(|ctx| app.draw_shortcuts_dialog(ctx));

    assert!(!app.shortcuts_open);
    assert_eq!(app.nav.section, section);
}

#[test]
fn shortcuts_dialog_renders_without_a_native_window() {
    let mut app = TeamsApp {
        shortcuts_open: true,
        ..Default::default()
    };

    render_headlessly(|ui| app.draw_shortcuts_dialog(ui.ctx()));

    assert!(app.shortcuts_open);
}

#[test]
fn chat_call_request_starts_the_shared_calls_lifecycle() {
    let mut app = TeamsApp::default();
    app.chat
        .request_call("Design review".into(), CallMode::Video);

    app.start_requested_chat_call();

    assert!(app.workspace.search_results("design").iter().any(|result| {
        matches!(
            &result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::CallContact(contact)
                if contact == "Design review"
        )
    }));
}

#[test]
fn resetting_a_signed_out_session_clears_local_workspace_state() {
    let mut app = TeamsApp::default();
    app.navigate_to(Section::Teams);
    app.navigate_to(Section::Calls);
    app.search.query_mut().push_str("session boundary");
    app.search.open();
    app.shortcuts_open = true;
    assert_eq!(
        app.workspace
            .start_chat_call("Session boundary contact", CallMode::Audio),
        Some("Session boundary contact".into())
    );
    assert!(
        app.workspace
            .search_results("session boundary")
            .iter()
            .any(|result| matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::CallContact(contact)
                    if contact == "Session boundary contact"
            ))
    );

    app.reset_local_session(&egui::Context::default());

    assert_eq!(app.nav.section, Section::Chat);
    assert_eq!(app.nav.history, [Section::Chat]);
    assert_eq!(app.nav.index, 0);
    assert!(!app.search.is_open());
    assert!(app.search.query_mut().is_empty());
    assert!(!app.shortcuts_open);
    assert!(app.workspace.search_results("session boundary").is_empty());
}

#[test]
fn production_persistence_flushes_only_dirty_chat_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalChatStore::at(directory.chat_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            chat_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    app.chat.select_workspace_conversation(1);
    app.stores.chat_store.make_save_due();

    app.flush_local_chat(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("chat snapshot must load")
        .expect("dirty chat must persist");
    assert!(
        snapshot
            .conversations
            .get(&crate::data::workspace::workspace().chat.conversations[1].id)
            .is_some_and(|conversation| conversation.read)
    );
    assert!(!app.chat.is_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_local_presence() {
    let directory = TemporaryDirectory::new();
    let store = LocalPresenceStore::at(directory.presence_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            presence_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(app.auth.set_local_presence(LocalPresence::DoNotDisturb));
    assert!(app.auth.is_presence_dirty());
    app.stores.presence_store.make_save_due();
    app.flush_local_presence(&egui::Context::default());

    assert_eq!(
        store.load().expect("presence snapshot must load"),
        Some(LocalPresence::DoNotDisturb)
    );
    assert!(!app.auth.is_presence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_calendar_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalCalendarStore::at(directory.calendar_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            calendar_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(app.workspace.start_channel_meeting());
    app.stores.calendar_store.make_save_due();

    app.flush_local_calendar(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("calendar snapshot must load")
        .expect("dirty calendar must persist");
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].title, "Bike Commuters · General meeting");
    assert!(!app.workspace.is_calendar_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_activity_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalActivityStore::at(directory.activity_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            activity_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let _ = app.workspace.select_search_result(
        crate::features::search::workspace::WorkspaceSearchTarget::Activity(0),
    );
    assert!(app.workspace.is_activity_persistence_dirty());
    app.stores.activity_store.make_save_due();

    app.flush_local_activity(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("Activity snapshot must load")
        .expect("dirty Activity state must persist");
    assert_eq!(snapshot.read_item_keys, vec!["parking-changes"]);
    assert!(!app.workspace.is_activity_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_teams_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalTeamsStore::at(directory.teams_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            teams_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    app.workspace
        .open_team_channel("Juniper Row All Hands · Facilities");
    assert!(app.workspace.is_teams_persistence_dirty());
    app.stores.teams_store.make_save_due();

    app.flush_local_teams(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("Teams snapshot must load")
        .expect("dirty Teams state must persist");
    assert_eq!(
        snapshot.selected_channel.as_deref(),
        Some("Juniper Row All Hands · Facilities")
    );
    assert!(!app.workspace.is_teams_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_canonical_discussion_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalThreadStore::at(directory.threads_path());
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture discussion id must be valid");
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            threads_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    app.workspace
        .threads_mut()
        .append_reply(thread.clone(), "Durable shared reply")
        .expect("fixture reply must be accepted");
    assert!(app.workspace.is_threads_persistence_dirty());
    app.stores.threads_store.make_save_due();

    app.flush_local_threads(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("discussion snapshot must load")
        .expect("dirty discussion state must persist");
    let restored = ThreadStore::from_stored(snapshot);
    assert_eq!(restored.replies(&thread)[0].body(), "Durable shared reply");
    assert!(!app.workspace.is_threads_persistence_dirty());
}

#[test]
fn canonical_migration_cleanup_waits_for_durable_markers() {
    let directory = TemporaryDirectory::new();
    let mut app = TeamsApp {
        stores: LocalStores {
            activity_store: LocalFeatureStore::with_writable(LocalActivityStore::at(
                directory.activity_path(),
            )),
            teams_store: LocalFeatureStore::with_writable(LocalTeamsStore::at(
                directory.teams_path(),
            )),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut legacy_activity = StoredActivity::default();
    legacy_activity.replies_by_item.insert(
        "parking-changes".into(),
        vec!["Activity cache must survive until canonical save.".into()],
    );
    let mut legacy_teams = StoredTeams::default();
    legacy_teams
        .channels
        .entry("Juniper Row All Hands · Facilities".into())
        .or_default()
        .replies_by_post
        .insert(
            "parking-changes".into(),
            vec!["Teams cache must survive until canonical save.".into()],
        );
    let mut partial_canonical = ThreadStore::default();
    partial_canonical
        .mark_activity_v1_imported()
        .expect("small marker must fit");
    partial_canonical.mark_persisted();
    app.workspace.restore_activity(legacy_activity);
    app.workspace.restore_teams(legacy_teams);
    app.workspace.restore_threads(partial_canonical.stored());

    app.workspace
        .import_unmigrated_legacy_threads()
        .expect("remaining Teams source must import");
    assert!(app.workspace.legacy_thread_imports_are_confirmed());
    assert!(app.workspace.is_threads_persistence_dirty());
    assert!(!app.thread_migration.markers_persisted);

    // This is the window between an in-memory import and its canonical
    // atomic save. A process crash here must leave both old source files
    // usable, so the cleanup helper is intentionally a no-op.
    app.clear_confirmed_legacy_thread_overlays();
    assert!(!app.workspace.teams().legacy_discussion().is_empty());
    assert!(!app.workspace.is_teams_persistence_dirty());

    app.workspace.mark_threads_persisted();
    app.thread_migration.markers_persisted = true;
    app.clear_confirmed_legacy_thread_overlays();
    assert!(app.workspace.teams().legacy_discussion().is_empty());
    assert!(app.workspace.is_teams_persistence_dirty());
}

#[test]
fn valid_canonical_discussions_recover_when_legacy_sources_are_protected() {
    let directory = TemporaryDirectory::new();
    let canonical_store = LocalThreadStore::at(directory.threads_path());
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture discussion id must be valid");
    let mut canonical = ThreadStore::default();
    canonical
        .append_reply(thread.clone(), "The canonical reply must remain visible.")
        .expect("fixture reply must be accepted");
    canonical
        .mark_activity_v1_imported()
        .expect("small Activity marker must fit");
    canonical
        .mark_teams_v1_imported()
        .expect("small Teams marker must fit");
    canonical.mark_persisted();
    canonical_store
        .save(&canonical.stored())
        .expect("canonical fixture must save");

    let mut app = TeamsApp::default();
    // These protected adapters model source recovery failures. They must
    // not prevent the independent canonical snapshot from loading.
    app.stores
        .activity_store
        .protect_existing_data(LocalActivityStore::at(directory.activity_path()));
    app.stores
        .teams_store
        .protect_existing_data(LocalTeamsStore::at(directory.teams_path()));
    app.recover_local_threads_store(canonical_store);

    assert!(app.stores.threads_store.is_writable());
    assert!(app.thread_migration.markers_persisted);
    assert_eq!(
        app.workspace.threads_mut().replies(&thread)[0].body(),
        "The canonical reply must remain visible."
    );
    assert!(!app.workspace.is_threads_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_calls_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalCallsStore::at(directory.calls_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            calls_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        app.workspace
            .start_chat_call("Durable Calls", CallMode::Audio),
        Some("Durable Calls".into())
    );
    assert!(app.workspace.is_calls_persistence_dirty());
    app.stores.calls_store.make_save_due();

    app.flush_local_calls(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("Calls snapshot must load")
        .expect("dirty Calls state must persist");
    assert_eq!(snapshot.local_call_history.len(), 1);
    assert_eq!(snapshot.local_call_history[0].contact, "Durable Calls");
    assert!(!app.workspace.is_calls_persistence_dirty());
}

#[test]
fn production_persistence_flushes_only_dirty_files_state() {
    let directory = TemporaryDirectory::new();
    let store = LocalFilesStore::at(directory.files_path());
    let path = directory.local_upload_path();
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            files_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        app.workspace
            .files_mut()
            .add_local_file_for_test(path.clone())
    );
    assert!(app.workspace.is_files_persistence_dirty());
    app.stores.files_store.make_save_due();

    app.flush_local_files(&egui::Context::default());

    let snapshot = store
        .load()
        .expect("Files snapshot must load")
        .expect("dirty Files state must persist");
    assert_eq!(snapshot.local_uploads.len(), 1);
    assert_eq!(snapshot.local_uploads[0].path, path.to_string_lossy());
    assert!(!app.workspace.is_files_persistence_dirty());
}

#[test]
fn resetting_a_local_profile_removes_its_durable_feature_caches() {
    let directory = TemporaryDirectory::new();
    let presence_store = LocalPresenceStore::at(directory.presence_path());
    let chat_store = LocalChatStore::at(directory.chat_path());
    let chat_prefs_store = crate::features::chat::LocalChatPrefsStore::at(
        directory.chat_path().with_file_name("chat-prefs-v1.json"),
    );
    let app_prefs_store = crate::app::app_prefs::LocalAppPrefsStore::at(
        directory.chat_path().with_file_name("app-prefs-v1.json"),
    );
    let calendar_store = LocalCalendarStore::at(directory.calendar_path());
    let activity_store = LocalActivityStore::at(directory.activity_path());
    let teams_store = LocalTeamsStore::at(directory.teams_path());
    let threads_store = LocalThreadStore::at(directory.threads_path());
    let calls_store = LocalCallsStore::at(directory.calls_path());
    let files_store = LocalFilesStore::at(directory.files_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            presence_store: LocalFeatureStore::with_writable(presence_store.clone()),
            chat_store: LocalFeatureStore::with_writable(chat_store.clone()),
            chat_prefs_store: LocalFeatureStore::with_writable(chat_prefs_store.clone()),
            app_prefs_store: LocalFeatureStore::with_writable(app_prefs_store.clone()),
            calendar_store: LocalFeatureStore::with_writable(calendar_store.clone()),
            activity_store: LocalFeatureStore::with_writable(activity_store.clone()),
            teams_store: LocalFeatureStore::with_writable(teams_store.clone()),
            threads_store: LocalFeatureStore::with_writable(threads_store.clone()),
            calls_store: LocalFeatureStore::with_writable(calls_store.clone()),
            files_store: LocalFeatureStore::with_writable(files_store.clone()),
        },
        ..Default::default()
    };
    presence_store
        .save(LocalPresence::Away)
        .expect("presence cache must save before sign-out");
    app.auth.restore_local_presence(LocalPresence::Away);
    app.chat.select_workspace_conversation(1);
    chat_store
        .save(&app.chat.stored())
        .expect("chat cache must save before sign-out");
    app.chat.set_send_read_receipts(true);
    chat_prefs_store
        .save(&app.chat.prefs())
        .expect("chat preferences must save before sign-out");
    app_prefs_store
        .save(crate::app::app_prefs::AppPrefs {
            theme: ThemePreference::Dark,
        })
        .expect("app preferences must save before sign-out");
    assert!(app.workspace.start_channel_meeting());
    calendar_store
        .save(&app.workspace.calendar_snapshot())
        .expect("calendar cache must save before sign-out");
    let _ = app.workspace.select_search_result(
        crate::features::search::workspace::WorkspaceSearchTarget::Activity(0),
    );
    activity_store
        .save(&app.workspace.activity_snapshot())
        .expect("Activity cache must save before sign-out");
    app.workspace
        .open_team_channel("Juniper Row All Hands · Facilities");
    teams_store
        .save(&app.workspace.teams_snapshot())
        .expect("Teams cache must save before sign-out");
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture discussion id must be valid");
    app.workspace
        .threads_mut()
        .append_reply(thread, "Clear this canonical discussion reply")
        .expect("fixture reply must be accepted");
    threads_store
        .save(&app.workspace.threads_snapshot())
        .expect("discussion cache must save before sign-out");
    calls_store
        .save(&StoredCalls::default())
        .expect("Calls cache must save before sign-out");
    files_store
        .save(&StoredFiles::default())
        .expect("Files cache must save before sign-out");

    app.reset_local_session(&egui::Context::default());

    assert_eq!(
        presence_store
            .load()
            .expect("cleared presence cache must load"),
        None
    );
    assert_eq!(chat_store.load().expect("cleared cache must load"), None);
    assert_eq!(
        chat_prefs_store
            .load()
            .expect("cleared chat preferences must load"),
        None
    );
    assert!(
        !app.chat.send_read_receipts(),
        "a new session never inherits the old profile's choice"
    );
    assert_eq!(
        app_prefs_store
            .load()
            .expect("cleared app preferences must load"),
        None
    );
    assert_eq!(
        calendar_store
            .load()
            .expect("cleared calendar cache must load"),
        None
    );
    assert_eq!(
        activity_store
            .load()
            .expect("cleared Activity cache must load"),
        None
    );
    assert_eq!(
        teams_store.load().expect("cleared Teams cache must load"),
        None
    );
    assert_eq!(
        threads_store
            .load()
            .expect("cleared discussion cache must load"),
        None
    );
    assert_eq!(
        calls_store.load().expect("cleared Calls cache must load"),
        None
    );
    assert_eq!(
        files_store.load().expect("cleared Files cache must load"),
        None
    );
    assert!(!app.stores.chat_store.has_store());
    assert!(!app.stores.chat_store.is_initialized());
    assert!(!app.stores.chat_store.is_writable());
    assert!(!app.chat.is_persistence_dirty());
    assert!(!app.stores.presence_store.has_store());
    assert!(!app.stores.presence_store.is_initialized());
    assert!(!app.stores.presence_store.is_writable());
    assert!(!app.stores.calendar_store.has_store());
    assert!(!app.stores.calendar_store.is_initialized());
    assert!(!app.stores.calendar_store.is_writable());
    assert!(!app.workspace.is_calendar_persistence_dirty());
    assert!(!app.stores.activity_store.has_store());
    assert!(!app.stores.activity_store.is_initialized());
    assert!(!app.stores.activity_store.is_writable());
    assert!(!app.workspace.is_activity_persistence_dirty());
    assert!(!app.stores.teams_store.has_store());
    assert!(!app.stores.teams_store.is_initialized());
    assert!(!app.stores.teams_store.is_writable());
    assert!(!app.workspace.is_teams_persistence_dirty());
    assert!(!app.stores.threads_store.has_store());
    assert!(!app.stores.threads_store.is_initialized());
    assert!(!app.stores.threads_store.is_writable());
    assert!(!app.workspace.is_threads_persistence_dirty());
    assert!(!app.stores.calls_store.has_store());
    assert!(!app.stores.calls_store.is_initialized());
    assert!(!app.stores.calls_store.is_writable());
    assert!(!app.workspace.is_calls_persistence_dirty());
    assert!(!app.stores.files_store.has_store());
    assert!(!app.stores.files_store.is_initialized());
    assert!(!app.stores.files_store.is_writable());
    assert!(!app.workspace.is_files_persistence_dirty());
}

#[test]
fn rail_with_overflow_controls_renders_without_a_native_window() {
    let ctx = egui::Context::default();
    crate::ui::theme::apply(&ctx);
    let mut app = TeamsApp::default();

    ctx.begin_pass(egui::RawInput::default());
    app.draw_rail(&ctx);
    let _ = ctx.end_pass();
}

#[test]
fn every_workspace_surface_renders_without_a_native_window() {
    let mut workspace = WorkspaceState::default();
    for section in [
        Section::Activity,
        Section::Teams,
        Section::Calendar,
        Section::Calls,
        Section::Files,
    ] {
        let mut toast = ToastState::default();
        render_headlessly(|ui| workspace.draw_sidebar(ui, section, &mut toast));
        render_headlessly(|ui| workspace.draw_page(ui, section, &mut toast));
    }

    let mut chat = ChatState::default();
    let mut toast = ToastState::default();
    render_headlessly(|ui| chat.draw_sidebar(ui, &mut toast));
    render_headlessly(|ui| chat.draw_conversation(ui, &mut toast));
}

#[test]
fn every_app_section_lays_out_at_the_declared_minimum_window_size() {
    for section in [
        Section::Activity,
        Section::Chat,
        Section::Teams,
        Section::Calendar,
        Section::Calls,
        Section::Files,
    ] {
        let mut app = TeamsApp {
            nav: Navigation::new(section),
            ..Default::default()
        };
        render_at_minimum_window_size(&mut app);
    }
}

#[test]
fn shell_modal_states_layout_at_the_declared_minimum_window_size() {
    let mut shortcuts = TeamsApp {
        shortcuts_open: true,
        ..Default::default()
    };
    render_at_minimum_window_size(&mut shortcuts);
    assert!(shortcuts.shortcuts_open);

    let mut account = TeamsApp::default();
    account.auth.toggle_profile();
    render_at_minimum_window_size(&mut account);
    assert!(account.auth.is_profile_open());
}

#[test]
fn sidebar_toggle_hides_and_restores_the_list_panel() {
    fn sidebar_width(app: &mut TeamsApp) -> f32 {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        ctx.begin_pass(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        });
        app.draw_sidebar(&ctx);
        let width = ctx.available_rect().left();
        let _ = ctx.end_pass();
        width
    }

    let mut app = TeamsApp::default();
    assert!(!app.sidebar_collapsed);
    assert!(sidebar_width(&mut app) >= super::SIDEBAR_WIDTH);

    app.sidebar_collapsed = true;
    assert_eq!(sidebar_width(&mut app), 0.0);

    app.sidebar_collapsed = false;
    assert!(sidebar_width(&mut app) >= super::SIDEBAR_WIDTH);
}

#[test]
fn flushing_for_exit_writes_dirty_state_without_waiting_for_the_debounce() {
    let directory = TemporaryDirectory::new();
    let store = LocalChatStore::at(directory.chat_path());
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            chat_store: LocalFeatureStore::with_writable(store.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    app.chat.select_workspace_conversation(1);

    // The debounce has not elapsed, so a normal flush would defer.
    app.flush_local_chat(&egui::Context::default());
    assert!(store.load().expect("load").is_none());

    let failures = app.flush_all_local_stores_now(&egui::Context::default());

    assert!(failures.is_empty());
    assert!(store.load().expect("load").is_some());
    assert!(!app.chat.is_persistence_dirty());
}

fn close_request_pass(app: &mut TeamsApp, ctx: &egui::Context) -> bool {
    let mut viewport = egui::ViewportInfo::default();
    viewport.events.push(egui::ViewportEvent::Close);
    ctx.begin_pass(egui::RawInput {
        viewports: [(egui::ViewportId::ROOT, viewport)].into_iter().collect(),
        ..Default::default()
    });
    app.handle_close_request(ctx);
    let output = ctx.end_pass();
    output.viewport_output[&egui::ViewportId::ROOT]
        .commands
        .iter()
        .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
}

#[test]
fn a_failed_exit_save_holds_the_first_close_request_and_warns_persistently() {
    let directory = TemporaryDirectory::new();
    // A directory in place of the snapshot file makes every save fail.
    let blocked = directory.chat_path();
    std::fs::create_dir_all(&blocked).expect("blocking directory");
    let mut app = TeamsApp {
        local_persistence_enabled: true,
        stores: LocalStores {
            chat_store: LocalFeatureStore::with_writable(LocalChatStore::at(blocked)),
            ..Default::default()
        },
        ..Default::default()
    };
    app.chat.select_workspace_conversation(1);
    let ctx = egui::Context::default();

    assert!(close_request_pass(&mut app, &ctx), "first close is held");
    let warnings = app.local_storage_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("chat data"), "{}", warnings[0]);

    assert!(
        !close_request_pass(&mut app, &ctx),
        "a second close request quits anyway"
    );
}

#[test]
fn a_clean_close_request_is_not_held_back() {
    let mut app = TeamsApp::default();
    assert!(!close_request_pass(&mut app, &egui::Context::default()));
}

#[test]
fn sign_out_drops_per_widget_text_edit_state() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("some_text_edit");
    ctx.data_mut(|data| data.insert_persisted(id, String::from("typed by profile one")));
    let mut app = TeamsApp {
        pending_local_storage_notice: Some("stale notice".into()),
        ..Default::default()
    };

    app.reset_local_session(&ctx);

    assert!(
        ctx.data_mut(|data| data.get_persisted::<String>(id))
            .is_none()
    );
    assert!(app.pending_local_storage_notice.is_none());
}

fn focus_pass(app: &mut TeamsApp, ctx: &egui::Context, focused: Option<bool>, f5: bool) {
    let viewport = egui::ViewportInfo {
        focused,
        ..Default::default()
    };
    let mut events = Vec::new();
    if f5 {
        events.push(egui::Event::Key {
            key: egui::Key::F5,
            physical_key: Some(egui::Key::F5),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
    }
    ctx.begin_pass(egui::RawInput {
        viewports: [(egui::ViewportId::ROOT, viewport)].into_iter().collect(),
        events,
        ..Default::default()
    });
    app.observe_window_focus(ctx);
    let _ = ctx.end_pass();
}

#[test]
fn the_focus_edge_feeds_one_signal_to_refresh_and_chat() {
    use crate::app::remote_refresh::RefreshTarget;
    let mut app = TeamsApp::default();
    let ctx = egui::Context::default();
    let now = std::time::Instant::now();

    // Startup (no focus event yet) counts as focused and is no edge.
    focus_pass(&mut app, &ctx, None, false);
    assert!(app.refresh.focused());
    assert!(app.refresh.take_plan(now, &[]).is_empty());

    focus_pass(&mut app, &ctx, Some(false), false);
    assert!(!app.refresh.focused());
    assert!(app.refresh.take_plan(now, &[]).is_empty());

    // Regaining focus signals exactly one directory refresh.
    focus_pass(&mut app, &ctx, Some(true), false);
    assert!(app.refresh.focused());
    assert_eq!(
        app.refresh.take_plan(now, &[]).targets(),
        [RefreshTarget::Directory]
    );
    focus_pass(&mut app, &ctx, Some(true), false);
    assert!(app.refresh.take_plan(now, &[]).is_empty());
}

#[test]
fn f5_is_an_explicit_refresh_but_not_while_unfocused() {
    use crate::app::remote_refresh::RefreshTarget;
    let mut app = TeamsApp::default();
    let ctx = egui::Context::default();
    let now = std::time::Instant::now();

    focus_pass(&mut app, &ctx, Some(true), true);
    let plan = app.refresh.take_plan(now, &[RefreshTarget::SelectedChat]);
    assert_eq!(plan.targets().len(), 2);

    focus_pass(&mut app, &ctx, Some(false), true);
    assert!(
        app.refresh
            .take_plan(now, &[RefreshTarget::SelectedChat])
            .is_empty()
    );
}

#[test]
fn a_read_receipt_outcome_is_silent_and_only_a_403_turns_receipts_off() {
    use crate::app::workspace_transport::CommandDispatch;
    use crate::integrations::{TransportError, WorkspaceIntentId};

    let mut app = TeamsApp::default();
    let intent = WorkspaceIntentId::from_sequence(7);
    let other = WorkspaceIntentId::from_sequence(8);
    let delivered = |intent_id| CommandDispatch::Delivered {
        intent_id,
        receipt: CommandReceipt {
            command_id: "microsoft365:chat-read-receipt".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        },
    };

    // An intent that is not a receipt is left to the ordinary handlers.
    assert!(!app.finish_read_receipt(&delivered(other)));

    for error in [
        TransportError::Throttled {
            retry_after_seconds: Some(5),
        },
        TransportError::Unavailable("uncertain".into()),
        TransportError::InvalidCommand("rejected".into()),
    ] {
        app.read_receipt_intents.push(intent);
        let outcome = CommandDispatch::Rejected {
            intent_id: intent,
            error,
        };
        assert!(app.finish_read_receipt(&outcome));
        assert!(app.read_receipt_intents.is_empty());
        assert!(app.chat.read_receipts_offered(), "not a 403");
    }
    app.read_receipt_intents.push(intent);
    assert!(app.finish_read_receipt(&delivered(intent)));
    assert!(app.chat.read_receipts_offered());

    app.read_receipt_intents.push(intent);
    let forbidden = CommandDispatch::Rejected {
        intent_id: intent,
        error: TransportError::Unauthorized,
    };
    assert!(app.finish_read_receipt(&forbidden));
    assert!(!app.chat.read_receipts_offered(), "403 is NotGranted");
}

#[test]
fn an_upload_outcome_is_consumed_silently_and_only_a_403_turns_attaching_off() {
    use crate::app::UploadIntent;
    use crate::app::workspace_transport::CommandDispatch;
    use crate::integrations::{TransportError, UploadedFileHandle, WorkspaceIntentId};

    let mut app = TeamsApp::default();
    let intent = WorkspaceIntentId::from_sequence(17);
    let other = WorkspaceIntentId::from_sequence(18);
    let delivered = |intent_id| CommandDispatch::Delivered {
        intent_id,
        receipt: CommandReceipt {
            command_id: "microsoft365:chat-upload".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(crate::integrations::UploadReceipt::Done(
                UploadedFileHandle::for_test(1, "q3.pdf", "153fa47d-18c9-4179-be08-9879815a9f90"),
            )),
        },
    };

    // An intent that is not an upload is left to the ordinary handlers.
    assert!(!app.finish_chat_upload(&delivered(other)));

    // An upload whose composer is gone is still consumed, never surfaced as
    // an "invalid workspace intent".
    for outcome in [
        delivered(intent),
        CommandDispatch::Rejected {
            intent_id: intent,
            error: TransportError::InvalidCommand("a file with that name".into()),
        },
        CommandDispatch::DeliveryUncertain {
            intent_id: intent,
            error: TransportError::Unavailable("uncertain".into()),
        },
        CommandDispatch::Rejected {
            intent_id: intent,
            error: TransportError::Cancelled,
        },
    ] {
        app.upload_intents.push(UploadIntent {
            intent,
            chat: crate::integrations::ProviderResourceRef::local_preview("chat"),
            cancel: false,
            share: false,
        });
        assert!(app.finish_chat_upload(&outcome));
        assert!(app.upload_intents.is_empty());
        assert!(app.chat.file_attach_offered(), "not a 403");
    }

    // A backoff keeps the intent waiting.
    app.upload_intents.push(UploadIntent {
        intent,
        chat: crate::integrations::ProviderResourceRef::local_preview("chat"),
        cancel: false,
        share: false,
    });
    let retry = CommandDispatch::RetryScheduled {
        intent_id: intent,
        error: TransportError::Throttled {
            retry_after_seconds: Some(5),
        },
        notify_user: false,
    };
    assert!(app.finish_chat_upload(&retry));
    assert_eq!(app.upload_intents.len(), 1);
    app.upload_intents.clear();

    app.upload_intents.push(UploadIntent {
        intent,
        chat: crate::integrations::ProviderResourceRef::local_preview("chat"),
        cancel: false,
        share: false,
    });
    let forbidden = CommandDispatch::Rejected {
        intent_id: intent,
        error: TransportError::Unauthorized,
    };
    assert!(app.finish_chat_upload(&forbidden));
    assert!(!app.chat.file_attach_offered(), "403 is NotGranted");
}

#[test]
fn a_fragment_nobody_waits_for_cancels_its_session_and_the_cancel_is_silent() {
    use crate::app::UploadIntent;
    use crate::app::workspace_transport::CommandDispatch;
    use crate::integrations::{
        ProviderResourceRef, TransportError, UploadProgress, UploadReceipt, WorkspaceIntentId,
    };

    let mut app = TeamsApp::default();
    let fragment = WorkspaceIntentId::from_sequence(31);
    app.upload_intents.push(UploadIntent {
        intent: fragment,
        chat: ProviderResourceRef::local_preview("chat"),
        cancel: false,
        share: false,
    });
    let progress = CommandDispatch::Delivered {
        intent_id: fragment,
        receipt: CommandReceipt {
            command_id: "microsoft365:chat-upload-fragment".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(UploadReceipt::Progress(UploadProgress::for_test(
                9, 3_932_160, 10_000_000,
            ))),
        },
    };
    // No composer owns it: the session is cancelled, and says nothing.
    assert!(app.finish_chat_upload(&progress));
    let cancels: Vec<_> = app
        .upload_intents
        .iter()
        .filter(|upload| upload.cancel)
        .collect();
    assert_eq!(cancels.len(), 1, "one cancel is queued");
    assert!(
        !app.upload_intents
            .iter()
            .any(|upload| upload.intent == fragment)
    );

    // The cancel's own outcome, success or failure, is consumed silently.
    let cancel = cancels[0].intent;
    for outcome in [
        CommandDispatch::Rejected {
            intent_id: cancel,
            error: TransportError::Unsupported("a Microsoft 365 connection is required".into()),
        },
        CommandDispatch::Delivered {
            intent_id: cancel,
            receipt: CommandReceipt {
                command_id: "microsoft365:chat-upload-cancel".into(),
                disposition: CommandDisposition::AppliedRemotely,
                upload: None,
            },
        },
    ] {
        let mut app = TeamsApp::default();
        app.upload_intents.push(UploadIntent {
            intent: cancel,
            chat: ProviderResourceRef::local_preview("chat"),
            cancel: true,
            share: false,
        });
        assert!(app.finish_chat_upload(&outcome));
        assert!(app.upload_intents.is_empty());
    }
}

#[test]
fn a_share_outcome_only_ever_warns_and_never_reaches_the_composer() {
    use crate::app::UploadIntent;
    use crate::app::workspace_transport::CommandDispatch;
    use crate::integrations::{
        ProviderResourceRef, ShareReport, TransportError, UploadReceipt, WorkspaceIntentId,
    };

    let intent = WorkspaceIntentId::from_sequence(41);
    let shared = |granted, failed| CommandDispatch::Delivered {
        intent_id: intent,
        receipt: CommandReceipt {
            command_id: "microsoft365:chat-file-share".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(UploadReceipt::Shared(ShareReport { granted, failed })),
        },
    };
    let run = |outcome: CommandDispatch| {
        let mut app = TeamsApp::default();
        app.upload_intents.push(UploadIntent {
            intent,
            chat: ProviderResourceRef::local_preview("chat"),
            cancel: false,
            share: true,
        });
        assert!(
            app.finish_chat_upload(&outcome),
            "a share is always consumed"
        );
        assert!(app.upload_intents.is_empty());
        assert!(
            app.chat.file_attach_offered(),
            "a share failure is not a 403"
        );
        app.toast.messages()
    };

    // Everyone was given access, or there was no one to give it to: silent.
    assert!(run(shared(3, 0)).is_empty());
    assert!(run(shared(0, 0)).is_empty());
    // A shortfall warns, with counts and no names.
    let partial = run(shared(2, 1));
    assert_eq!(partial.len(), 1);
    assert!(partial[0].contains("1 of 3") && partial[0].contains("may not be able to open"));
    let nobody = run(shared(0, 4));
    assert!(nobody[0].contains("could not be shared") && nobody[0].contains("may not be able"));
    // A rejected, throttled or uncertain share warns too, and nothing is retried.
    for error in [
        TransportError::Unauthorized,
        TransportError::Throttled {
            retry_after_seconds: Some(3),
        },
        TransportError::Unavailable("uncertain".into()),
        TransportError::InvalidCommand("gone".into()),
    ] {
        let rejected = run(CommandDispatch::Rejected {
            intent_id: intent,
            error: error.clone(),
        });
        assert_eq!(rejected.len(), 1, "{error:?}");
        let uncertain = run(CommandDispatch::DeliveryUncertain {
            intent_id: intent,
            error,
        });
        assert_eq!(uncertain.len(), 1);
    }
}

#[test]
fn no_read_receipt_is_staged_without_a_remote_session() {
    let mut app = TeamsApp::default();
    app.chat.set_send_read_receipts(true);
    app.send_read_receipt_if_due();
    assert!(app.read_receipt_intents.is_empty());
}

mod os_notifications {
    use super::*;
    use crate::app::os_notify::{Identity, Os, OsNotifier, Platform};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct Linux;

    impl Platform for Linux {
        fn os(&self) -> Os {
            Os::Linux
        }

        fn main_bundle_id(&self) -> Option<String> {
            None
        }

        fn aumid_registered(&self, _: &str) -> bool {
            false
        }
    }

    fn app_with_recorder(
        available_choice: bool,
    ) -> (TeamsApp, Arc<Mutex<Vec<String>>>, egui::Context) {
        let shown = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&shown);
        let mut app = TeamsApp {
            os_notifier: OsNotifier::start_with(&Linux, move |_: Identity| {
                move |text: &str| sink.lock().expect("sink").push(text.to_owned())
            }),
            ..Default::default()
        };
        app.chat
            .set_os_notifications_available(app.os_notifier.is_available());
        app.chat.set_os_notifications_enabled(available_choice);
        (app, shown, egui::Context::default())
    }

    /// One arrival in a seeded remote chat, then the app's notification step.
    fn arrive(app: &mut TeamsApp, focused: bool, newest: &str, at: &str) {
        let chat = |id: &str, at: &str| {
            RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
                "19:os@thread.v2",
                RemoteChatKind::OneOnOne,
                "Private name",
                "Topic",
                "PN",
                true,
                vec![RemoteChatMessage::for_test_with_reactions(
                    id,
                    "Ada",
                    at,
                    "Secret message body",
                    false,
                    false,
                    Vec::new(),
                )],
            )])
        };
        app.refresh.observe_focus(focused);
        app.chat.set_window_focused(focused);
        app.chat.apply_remote_projection(chat(newest, at));
        app.announce_new_messages();
    }

    fn settle() {
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn an_unfocused_arrival_with_the_choice_on_reaches_the_system_with_count_only_text() {
        let (mut app, shown, _ctx) = app_with_recorder(true);
        app.chat
            .apply_remote_projection(RemoteChatProjection::for_test(vec![
                RemoteChatConversation::for_test(
                    "19:os@thread.v2",
                    RemoteChatKind::OneOnOne,
                    "Private name",
                    "Topic",
                    "PN",
                    true,
                    vec![RemoteChatMessage::for_test_with_reactions(
                        "m0",
                        "Ada",
                        "2026-11-06T09:00:00Z",
                        "Old",
                        false,
                        false,
                        Vec::new(),
                    )],
                ),
            ]));
        let _ = app.chat.take_notification_toast();
        arrive(&mut app, false, "m1", "2026-11-06T10:30:00Z");
        settle();
        let shown = shown.lock().expect("shown");
        assert_eq!(*shown, ["New Microsoft 365 message"]);
        assert!(
            !shown[0].contains("Secret") && !shown[0].contains("Private"),
            "never a body or, by default, a name"
        );
    }

    #[test]
    fn nothing_reaches_the_system_when_focused_or_when_the_choice_is_off() {
        // The last row is the positive control: the same arrival is delivered.
        for (focused, choice, delivered) in [(true, true, 0), (false, false, 0), (false, true, 1)] {
            let (mut app, shown, _ctx) = app_with_recorder(choice);
            arrive(&mut app, focused, "m0", "2026-11-06T10:30:00Z");
            arrive(&mut app, focused, "m1", "2026-11-06T10:40:00Z");
            settle();
            assert_eq!(
                shown.lock().expect("shown").len(),
                delivered,
                "focused={focused} choice={choice}"
            );
        }
    }

    #[test]
    fn a_default_app_has_no_system_notifier_at_all() {
        let app = TeamsApp::default();
        assert!(!app.os_notifier.is_available());
        assert!(!app.chat.os_notifications_available());
    }
}
