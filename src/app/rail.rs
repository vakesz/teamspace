//! The persistent app rail and the shared shell icon button.
//!
//! Keeping this chrome separate from the app lifecycle makes navigation
//! affordances easy to find without coupling them to feature rendering.

use super::{RAIL_DESTINATIONS, RAIL_ITEM_SIZE, RAIL_WIDTH, TeamsApp};
use crate::shared::model::Section;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, IconSize};
use crate::ui::theme::{self, font_size, radius, space};
use eframe::egui::{
    self, Align, CornerRadius, Layout, Popup, RichText, Sense, Stroke, WidgetInfo, WidgetType,
};

/// Width of the left-edge selection indicator, as in the Teams rail.
const INDICATOR_WIDTH: f32 = 3.0;
const INDICATOR_HEIGHT: f32 = 20.0;
/// Side of a square shell icon button (top bar, rail overflow items).
pub(super) const CHROME_BUTTON_SIZE: f32 = 32.0;

/// Largest count shown on a rail badge before it reads "99+".
const BADGE_CAP: usize = 99;
const BADGE_MIN: f32 = 16.0;
const BADGE_PAD: f32 = 8.0;

/// The badge text for `count`: nothing for 0, the number up to 99, else "99+".
fn badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=BADGE_CAP => Some(count.to_string()),
        _ => Some(format!("{BADGE_CAP}+")),
    }
}

/// The accessible name of a rail destination, with its unread count.
fn rail_label(label: &str, unread: usize) -> String {
    match badge_text(unread) {
        Some(count) => format!("{label}, {count} unread"),
        None => label.to_owned(),
    }
}

/// Paints the unread pill on an icon's top-right corner.
fn paint_badge(ui: &egui::Ui, icon_rect: egui::Rect, text: &str) {
    if !ui.is_rect_visible(icon_rect) {
        return;
    }
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        theme::semibold(font_size::MICRO),
        theme::fg_on_brand(),
    );
    let size = egui::vec2((galley.size().x + BADGE_PAD).max(BADGE_MIN), BADGE_MIN);
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            icon_rect.right() - size.x * 0.6,
            icon_rect.top() - size.y * 0.2,
        ),
        size,
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(255), theme::danger());
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        theme::fg_on_brand(),
    );
}

fn rail_icon(section: Section) -> Icon {
    match section {
        Section::Activity => Icon::Activity,
        Section::Chat => Icon::Chat,
        Section::Teams => Icon::Teams,
        Section::Calendar => Icon::Calendar,
        Section::Calls => Icon::Call,
        Section::Files => Icon::Files,
    }
}

/// Paints a hover-highlighted icon into `rect`. Disabled buttons use the
/// disabled foreground and neither highlight nor respond to clicks.
fn paint_icon_button(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: &egui::Response,
    icon: Icon,
    size: IconSize,
    selected: bool,
    enabled: bool,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    if enabled && response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius::SM), theme::surface_hover());
    }
    let tint = if !enabled {
        theme::fg_disabled()
    } else if selected {
        theme::brand_fg()
    } else {
        theme::fg2()
    };
    let image = if selected {
        icon.image_filled(size)
    } else {
        icon.image(size)
    };
    let side = size.px();
    image.tint(tint).paint_at(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::vec2(side, side)),
    );
}

/// A 32px shell icon button with an accessible label that, unlike
/// `icon_button`, can render disabled.
pub(super) fn chrome_icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    enabled: bool,
    tooltip: &str,
) -> egui::Response {
    let enabled = enabled && ui.is_enabled();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(CHROME_BUTTON_SIZE, CHROME_BUTTON_SIZE),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, tooltip));
    paint_icon_button(ui, rect, &response, icon, IconSize::M20, false, enabled);
    response.on_hover_text(tooltip)
}

impl TeamsApp {
    /// Unread items behind a rail destination: local activity items for
    /// Activity, and unread chats (catalog plus Microsoft 365, reusing the
    /// feature's own read state) for Chat. Other destinations have none.
    fn rail_unread_count(&self, section: Section) -> usize {
        match section {
            Section::Activity => self.workspace.activity_unread_count(),
            Section::Chat => self.chat.unread_chat_count(),
            _ => 0,
        }
    }

    pub(super) fn draw_rail(&mut self, ctx: &egui::Context) {
        let navigation_enabled = self.navigation_enabled();
        egui::SidePanel::left("rail")
            .exact_width(RAIL_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::rail_tint())
                    .inner_margin(egui::Margin::symmetric(0, space::MD as i8))
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle())),
            )
            .show(ctx, |ui| {
                let mut overflow_navigation = None;
                let mut show_about = false;
                let panel_left = ui.max_rect().left();
                ui.vertical_centered(|ui| {
                    for (section, label, shortcut) in RAIL_DESTINATIONS {
                        let selected = self.nav.section == section;
                        let unread = self.rail_unread_count(section);
                        let accessible_label = rail_label(label, unread);
                        let tooltip = format!(
                            "{accessible_label} ({})",
                            crate::ui::shortcuts::primary_chord(shortcut.name())
                        );
                        let (rect, response) = ui.allocate_exact_size(
                            egui::vec2(RAIL_ITEM_SIZE, RAIL_ITEM_SIZE),
                            Sense::click(),
                        );
                        response.widget_info(|| {
                            WidgetInfo::labeled(WidgetType::Button, true, &accessible_label)
                        });
                        paint_icon_button(
                            ui,
                            rect.shrink(space::XS),
                            &response,
                            rail_icon(section),
                            IconSize::L24,
                            selected,
                            true,
                        );
                        if let Some(text) = badge_text(unread) {
                            paint_badge(ui, rect.shrink(space::XS), &text);
                        }
                        if selected && ui.is_rect_visible(rect) {
                            let bar = egui::Rect::from_center_size(
                                egui::pos2(panel_left + INDICATOR_WIDTH / 2.0, rect.center().y),
                                egui::vec2(INDICATOR_WIDTH, INDICATOR_HEIGHT),
                            );
                            ui.painter().rect_filled(
                                bar,
                                CornerRadius::same(2),
                                theme::brand_fg(),
                            );
                        }
                        let response = response.on_hover_text(tooltip);
                        if navigation_enabled && response.clicked() {
                            self.navigate_to(section);
                        }
                        ui.add_space(space::XXS);
                    }
                    ui.add_space(space::SM);
                    ui.add_enabled_ui(navigation_enabled, |ui| {
                        let apps = chrome_icon_button(ui, Icon::Apps, true, "Apps");
                        Popup::menu(&apps).show(|ui| {
                            ui.label(
                                RichText::new("Open a local app")
                                    .font(theme::semibold(font_size::CAPTION))
                                    .color(theme::fg1()),
                            );
                            ui.separator();
                            for (section, label, _) in RAIL_DESTINATIONS {
                                if ui.button(label).clicked() {
                                    overflow_navigation = Some(section);
                                    ui.close();
                                }
                            }
                        });
                    });
                    ui.add_enabled_ui(navigation_enabled, |ui| {
                        let more = chrome_icon_button(ui, Icon::More, true, "More");
                        Popup::menu(&more).show(|ui| {
                            ui.label(
                                RichText::new("Local preview")
                                    .font(theme::regular(font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                            if ui.button("Keyboard shortcuts").clicked() {
                                self.shortcuts_open = true;
                                ui.close();
                            }
                            if ui.button("About this preview").clicked() {
                                show_about = true;
                                ui.close();
                            }
                        });
                    });
                });
                ui.allocate_ui_with_layout(
                    ui.available_size(),
                    Layout::bottom_up(Align::Center),
                    |ui| {
                        let presence = self.auth.local_presence();
                        let side = AvatarSize::Md32.px();
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(side, side), Sense::click());
                        Avatar::new(self.auth.display_name(), AvatarSize::Md32)
                            .presence(presence.avatar_presence())
                            .paint(ui, rect);
                        let tooltip = format!("{} (local preview) · Account", presence.label());
                        response.widget_info(|| {
                            WidgetInfo::labeled(WidgetType::Button, true, &tooltip)
                        });
                        if navigation_enabled && response.on_hover_text(tooltip).clicked() {
                            self.toggle_profile_menu();
                        }
                    },
                );
                if let Some(section) = overflow_navigation {
                    self.navigate_to(section);
                }
                if show_about {
                    self.toast.show(
                        "Local workspace data stays on this device. Microsoft 365 chat reads and plain-text sends are optional; other collaboration sync is not enabled.",
                    );
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::{badge_text, rail_label};

    #[test]
    fn badges_cap_at_ninety_nine_plus_and_hide_at_zero() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(99).as_deref(), Some("99"));
        assert_eq!(badge_text(100).as_deref(), Some("99+"));
        assert_eq!(badge_text(5_000).as_deref(), Some("99+"));
    }

    #[test]
    fn the_accessible_name_includes_the_unread_count_only_when_there_is_one() {
        assert_eq!(rail_label("Chat", 0), "Chat");
        assert_eq!(rail_label("Chat", 3), "Chat, 3 unread");
        assert_eq!(rail_label("Activity", 250), "Activity, 99+ unread");
    }
}
