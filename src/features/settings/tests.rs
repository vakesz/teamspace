use super::*;
use crate::ui::shortcuts::groups;
use eframe::egui::{self, Key};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

fn view(shortcuts: &[ShortcutGroup]) -> SettingsView<'_> {
    SettingsView {
        theme: ThemePreference::System,
        notifications: true,
        show_chat_name: false,
        os_notifications_available: true,
        os_notifications: false,
        read_receipts: false,
        read_receipts_offered: true,
        microsoft365: Microsoft365View {
            connected: true,
            cloud: "Global",
            account: Some("Ada Lovelace".into()),
            capabilities: RemoteCapabilities {
                chat_message_edit: Capability::Available,
                chat_message_delete: Capability::NotGranted,
                member_presence: Capability::Available,
                chat_mark_read: Capability::NotGranted,
                file_attach: Capability::Available,
            },
        },
        shortcuts,
    }
}

struct Fixture {
    state: SettingsState,
    view_theme: ThemePreference,
    connected: bool,
    receipts_offered: bool,
    actions: Vec<SettingsAction>,
    groups: Vec<ShortcutGroup>,
}

/// A harness whose dialog has finished its opening animation, so node
/// positions are stable before the first click.
fn harness(section: SettingsSection) -> Harness<'static, Fixture> {
    let mut state = SettingsState::default();
    state.open_section(section);
    let mut installed = false;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_state(
            move |ctx, fixture: &mut Fixture| {
                if !installed {
                    // Fonts take effect on the next pass.
                    theme::apply(ctx);
                    installed = true;
                    ctx.request_discard("install theme fonts");
                    return;
                }
                let mut view = view(&fixture.groups);
                view.theme = fixture.view_theme;
                view.microsoft365.connected = fixture.connected;
                view.read_receipts_offered = fixture.receipts_offered;
                let actions = fixture.state.draw(ctx, &view);
                fixture.actions.extend(actions);
            },
            Fixture {
                state,
                view_theme: ThemePreference::System,
                connected: true,
                receipts_offered: true,
                actions: Vec::new(),
                groups: groups(&[("1".into(), "Activity".into())]),
            },
        );
    harness.run_steps(6);
    harness
}

#[test]
fn a_closed_dialog_draws_nothing_and_returns_no_actions() {
    let mut harness = harness(SettingsSection::Appearance);
    harness.state_mut().state.close();
    harness.run();
    assert!(harness.query_by_label("Settings").is_none());
    assert!(harness.state().actions.is_empty());
}

#[test]
fn choosing_a_theme_emits_one_action_and_the_current_choice_emits_none() {
    let mut harness = harness(SettingsSection::Appearance);
    harness.run();
    harness.get_by_label("System").click();
    harness.run();
    assert!(harness.state().actions.is_empty());
    harness.get_by_label("Dark").click();
    harness.run();
    assert_eq!(
        harness.state().actions,
        vec![SettingsAction::SetTheme(ThemePreference::Dark)]
    );
}

#[test]
fn notification_toggles_emit_their_actions() {
    let mut harness = harness(SettingsSection::Notifications);
    harness.run();
    harness.get_by_label("Show chat names").click();
    harness
        .get_by_label("Also use system notifications")
        .click();
    harness.get_by_label("Notify me of new messages").click();
    harness.run();
    assert_eq!(
        harness.state().actions,
        vec![
            SettingsAction::SetShowChatName(true),
            SettingsAction::SetOsNotifications(true),
            SettingsAction::SetNotifications(false),
        ]
    );
}

#[test]
fn the_microsoft_365_section_lists_capabilities_and_disconnects() {
    let mut harness = harness(SettingsSection::Microsoft365);
    harness.run();
    assert!(harness.query_by_label("Ada Lovelace").is_some());
    assert!(harness.query_by_label("Delete own messages").is_some());
    // Delete and read receipts are not granted in the fixture.
    assert_eq!(harness.get_all_by_label("Not granted").count(), 2);
    assert_eq!(harness.get_all_by_label("Available").count(), 3);
    harness.get_by_label("Disconnect Microsoft 365").click();
    harness.run();
    assert_eq!(
        harness.state().actions,
        vec![SettingsAction::DisconnectMicrosoft365]
    );
}

#[test]
fn a_disconnected_account_offers_no_capabilities_or_disconnect() {
    let mut harness = harness(SettingsSection::Microsoft365);
    harness.state_mut().connected = false;
    harness.run();
    assert!(harness.query_by_label("Not connected").is_some());
    assert!(harness.query_by_label("Disconnect Microsoft 365").is_none());
    assert!(harness.query_by_label("Delete own messages").is_none());
}

#[test]
fn read_receipts_are_disabled_when_the_capability_is_not_offered() {
    let mut harness = harness(SettingsSection::Privacy);
    harness.state_mut().receipts_offered = false;
    harness.run();
    harness.get_by_label("Send read receipts").click();
    harness.run();
    assert!(harness.state().actions.is_empty());

    let mut harness = self::harness(SettingsSection::Privacy);
    harness.run();
    harness.get_by_label("Send read receipts").click();
    harness.run();
    assert_eq!(
        harness.state().actions,
        vec![SettingsAction::SetReadReceipts(true)]
    );
}

#[test]
fn the_shortcuts_section_lists_the_shared_groups() {
    let mut harness = harness(SettingsSection::Shortcuts);
    harness.run();
    assert!(harness.query_by_label("Open settings").is_some());
    assert!(harness.query_by_label("Activity").is_some());
}

#[test]
fn about_shows_the_version_and_both_licenses() {
    let mut harness = harness(SettingsSection::About);
    harness.run();
    assert!(
        harness
            .query_by_label(&format!("Teamspace {VERSION}"))
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("SIL Open Font License")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("Fluent UI System Icons")
            .is_some()
    );
}

#[test]
fn the_section_list_switches_content() {
    let mut harness = harness(SettingsSection::Appearance);
    harness.run();
    harness.get_by_label("Privacy").click();
    harness.run();
    assert_eq!(harness.state().state.section(), SettingsSection::Privacy);
}

#[test]
fn escape_and_close_dismiss_the_dialog() {
    let mut harness = harness(SettingsSection::Appearance);
    harness.run();
    harness.key_press(Key::Escape);
    harness.run();
    assert!(!harness.state().state.is_open());

    let mut harness = self::harness(SettingsSection::Appearance);
    harness.run();
    harness.get_by_label("Close").click();
    harness.run();
    assert!(!harness.state().state.is_open());
}

#[test]
fn the_vendored_license_texts_are_not_empty() {
    assert!(INTER_LICENSE.contains("SIL OPEN FONT LICENSE"));
    assert!(FLUENT_ICONS_LICENSE.contains("MIT"));
}
