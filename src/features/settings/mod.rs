//! The Settings dialog: app and per-profile choices in one place.
//!
//! Rendering is separate from state ownership. The shell builds a
//! [`SettingsView`] from the live preferences and capabilities each frame, and
//! applies the [`SettingsAction`]s that [`SettingsState::draw`] returns, so
//! this module never touches a store and stays testable without the app.

use crate::integrations::{Capability, RemoteCapabilities};
use crate::ui::modal;
use crate::ui::shortcuts::{ShortcutGroup, take_escape};
use crate::ui::theme::{self, ThemePreference};
use eframe::egui::{self, Align, Layout, RichText};

const DIALOG_WIDTH: f32 = 640.0;
const NAV_WIDTH: f32 = 150.0;
const CONTENT_WIDTH: f32 = 440.0;
const CONTENT_HEIGHT: f32 = 400.0;
/// Room kept for the section heading above the scrolling content.
const SECTION_HEADING_HEIGHT: f32 = 32.0;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const INTER_LICENSE: &str = include_str!("../../../assets/fonts/OFL.txt");
const FLUENT_ICONS_LICENSE: &str = include_str!("../../../assets/icons/LICENSE");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Appearance,
    Notifications,
    Microsoft365,
    Privacy,
    Shortcuts,
    About,
}

impl SettingsSection {
    const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Notifications,
        Self::Microsoft365,
        Self::Privacy,
        Self::Shortcuts,
        Self::About,
    ];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::Microsoft365 => "Microsoft 365",
            Self::Privacy => "Privacy",
            Self::Shortcuts => "Keyboard shortcuts",
            Self::About => "About",
        }
    }
}

/// A change the user asked for. The shell owns the stores and applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsAction {
    SetTheme(ThemePreference),
    SetNotifications(bool),
    SetShowChatName(bool),
    SetOsNotifications(bool),
    SetReadReceipts(bool),
    DisconnectMicrosoft365,
}

/// What the Microsoft 365 section shows. Never carries a token or scope text.
pub(crate) struct Microsoft365View {
    pub(crate) connected: bool,
    pub(crate) cloud: &'static str,
    pub(crate) account: Option<String>,
    pub(crate) capabilities: RemoteCapabilities,
}

/// Read-only snapshot of everything Settings displays.
pub(crate) struct SettingsView<'a> {
    pub(crate) theme: ThemePreference,
    pub(crate) notifications: bool,
    pub(crate) show_chat_name: bool,
    pub(crate) os_notifications_available: bool,
    pub(crate) os_notifications: bool,
    pub(crate) read_receipts: bool,
    pub(crate) read_receipts_offered: bool,
    pub(crate) microsoft365: Microsoft365View,
    pub(crate) shortcuts: &'a [ShortcutGroup],
}

/// Transient dialog state. Never persisted.
#[derive(Debug)]
pub(crate) struct SettingsState {
    open: bool,
    section: SettingsSection,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            open: false,
            section: SettingsSection::Appearance,
        }
    }
}

impl SettingsState {
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub(crate) const fn section(&self) -> SettingsSection {
        self.section
    }

    pub(crate) const fn open_section(&mut self, section: SettingsSection) {
        self.open = true;
        self.section = section;
    }

    pub(crate) const fn close(&mut self) {
        self.open = false;
    }

    /// Draws the dialog and returns the changes made this frame.
    pub(crate) fn draw(
        &mut self,
        ctx: &egui::Context,
        view: &SettingsView<'_>,
    ) -> Vec<SettingsAction> {
        let mut actions = Vec::new();
        if !self.open {
            return actions;
        }
        if take_escape(ctx) {
            self.open = false;
            return actions;
        }
        let mut close = false;
        let _ = modal::show(ctx, "settings", DIALOG_WIDTH, |ui| {
            modal::heading(ui, "Settings");
            ui.add_space(theme::space::MD);
            // A fixed-height row: a separator in an unbounded horizontal
            // layout would stretch to the whole window height.
            let row = egui::vec2(NAV_WIDTH + CONTENT_WIDTH + 24.0, CONTENT_HEIGHT);
            ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Min), |ui| {
                ui.vertical(|ui| {
                    ui.set_width(NAV_WIDTH);
                    for section in SettingsSection::ALL {
                        if ui
                            .selectable_label(self.section == section, section.title())
                            .clicked()
                        {
                            self.section = section;
                        }
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_width(CONTENT_WIDTH);
                    section_heading(ui, self.section.title());
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .max_height(CONTENT_HEIGHT - SECTION_HEADING_HEIGHT)
                        .show(ui, |ui| match self.section {
                            SettingsSection::Appearance => appearance(ui, view, &mut actions),
                            SettingsSection::Notifications => {
                                notifications(ui, view, &mut actions);
                            }
                            SettingsSection::Microsoft365 => {
                                microsoft365(ui, view, &mut actions);
                            }
                            SettingsSection::Privacy => privacy(ui, view, &mut actions),
                            SettingsSection::Shortcuts => shortcuts(ui, view),
                            SettingsSection::About => about(ui),
                        });
                });
            });
            ui.add_space(theme::space::LG);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.open = false;
        }
        actions
    }
}

fn section_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        RichText::new(title)
            .font(theme::semibold(theme::font_size::BODY))
            .color(theme::fg1()),
    );
    ui.add_space(theme::space::SM);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::regular(theme::font_size::CAPTION))
            .color(theme::fg3()),
    );
}

fn appearance(ui: &mut egui::Ui, view: &SettingsView<'_>, actions: &mut Vec<SettingsAction>) {
    for choice in ThemePreference::ALL {
        if ui.radio(view.theme == choice, choice.label()).clicked() && view.theme != choice {
            actions.push(SettingsAction::SetTheme(choice));
        }
    }
    ui.add_space(theme::space::SM);
    note(ui, "System follows your operating system's appearance.");
}

fn toggle(
    ui: &mut egui::Ui,
    current: bool,
    enabled: bool,
    label: &str,
    hint: &str,
    action: fn(bool) -> SettingsAction,
    actions: &mut Vec<SettingsAction>,
) {
    let mut value = current;
    ui.add_enabled(enabled, egui::Checkbox::new(&mut value, label))
        .on_hover_text(hint);
    if value != current {
        actions.push(action(value));
    }
}

fn notifications(ui: &mut egui::Ui, view: &SettingsView<'_>, actions: &mut Vec<SettingsAction>) {
    toggle(
        ui,
        view.notifications,
        true,
        "Notify me of new messages",
        "In-app badges and toasts. They never show message text.",
        SettingsAction::SetNotifications,
        actions,
    );
    toggle(
        ui,
        view.show_chat_name,
        view.notifications,
        "Show chat names",
        "Off: notifications show only a count.",
        SettingsAction::SetShowChatName,
        actions,
    );
    if view.os_notifications_available {
        toggle(
            ui,
            view.os_notifications,
            view.notifications,
            "Also use system notifications",
            "When the window is not focused. Count only, never message text.",
            SettingsAction::SetOsNotifications,
            actions,
        );
    }
    ui.add_space(theme::space::SM);
    note(
        ui,
        "These apply to Microsoft 365 chats. Mute a single chat from its ... menu.",
    );
}

const fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::Available => "Available",
        Capability::NotGranted => "Not granted",
        Capability::UnsupportedInCloud => "Not available in this cloud",
        Capability::Unknown => "Not confirmed",
    }
}

fn microsoft365(ui: &mut egui::Ui, view: &SettingsView<'_>, actions: &mut Vec<SettingsAction>) {
    let m365 = &view.microsoft365;
    egui::Grid::new("settings_m365_summary")
        .num_columns(2)
        .spacing(egui::vec2(24.0, 6.0))
        .show(ui, |ui| {
            ui.label("Cloud");
            ui.label(m365.cloud);
            ui.end_row();
            ui.label("Account");
            ui.label(if m365.connected {
                m365.account.as_deref().unwrap_or("Connected")
            } else {
                "Not connected"
            });
            ui.end_row();
        });
    ui.add_space(theme::space::LG);
    if !m365.connected {
        note(
            ui,
            "Connect from the More options menu in the top bar. Optional permissions are listed here once you are connected.",
        );
        return;
    }
    ui.label(
        RichText::new("Optional permissions")
            .font(theme::semibold(theme::font_size::CAPTION))
            .color(theme::fg1()),
    );
    let capabilities = m365.capabilities;
    egui::Grid::new("settings_m365_capabilities")
        .num_columns(2)
        .spacing(egui::vec2(24.0, 6.0))
        .show(ui, |ui| {
            for (label, capability) in [
                ("Edit own messages", capabilities.chat_message_edit),
                ("Delete own messages", capabilities.chat_message_delete),
                ("Read receipts", capabilities.chat_mark_read),
                ("Presence", capabilities.member_presence),
                ("Attach files", capabilities.file_attach),
            ] {
                ui.label(label);
                ui.label(capability_label(capability));
                ui.end_row();
            }
        });
    ui.add_space(theme::space::LG);
    if ui.button("Disconnect Microsoft 365").clicked() {
        actions.push(SettingsAction::DisconnectMicrosoft365);
    }
}

fn privacy(ui: &mut egui::Ui, view: &SettingsView<'_>, actions: &mut Vec<SettingsAction>) {
    toggle(
        ui,
        view.read_receipts,
        view.read_receipts_offered,
        "Send read receipts",
        "Lets people in a chat see that you opened it. Off by default.",
        SettingsAction::SetReadReceipts,
        actions,
    );
    ui.add_space(theme::space::SM);
    note(
        ui,
        if view.read_receipts_offered {
            "Receipts are visible to the other people in the chat."
        } else {
            "Unavailable: the optional Chat.ReadWrite permission was not granted."
        },
    );
}

fn shortcuts(ui: &mut egui::Ui, view: &SettingsView<'_>) {
    for (index, group) in view.shortcuts.iter().enumerate() {
        if index > 0 {
            ui.add_space(theme::space::LG);
        }
        ui.label(
            RichText::new(&group.title)
                .font(theme::semibold(theme::font_size::CAPTION))
                .color(theme::fg1()),
        );
        egui::Grid::new(("settings_shortcuts", index))
            .num_columns(2)
            .spacing(egui::vec2(26.0, 6.0))
            .show(ui, |ui| {
                for (keys, action) in &group.rows {
                    ui.label(RichText::new(keys).color(theme::brand_fg()));
                    ui.label(action);
                    ui.end_row();
                }
            });
    }
}

fn about(ui: &mut egui::Ui) {
    ui.label(format!("Teamspace {VERSION}"));
    ui.add_space(theme::space::SM);
    note(
        ui,
        "Local workspace data stays on this device. Microsoft 365 access is optional.",
    );
    ui.add_space(theme::space::LG);
    ui.label(
        RichText::new("Licenses")
            .font(theme::semibold(theme::font_size::CAPTION))
            .color(theme::fg1()),
    );
    for (name, text) in [
        ("Inter (SIL Open Font License 1.1)", INTER_LICENSE),
        ("Fluent UI System Icons (MIT)", FLUENT_ICONS_LICENSE),
    ] {
        egui::CollapsingHeader::new(name).show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(text)
                            .font(theme::regular(theme::font_size::SMALL))
                            .color(theme::fg3()),
                    );
                });
        });
    }
}

#[cfg(test)]
mod tests;
