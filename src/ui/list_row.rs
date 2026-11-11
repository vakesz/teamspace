//! Sidebar list rows and collapsible section headers.

use super::avatar::Avatar;
use super::icon::{Icon, IconSize};
use super::theme;
use eframe::egui::{
    self, Color32, CornerRadius, FontId, Galley, Response, Sense, Ui, text::LayoutJob,
};
use std::sync::Arc;

const ROW_HEIGHT: f32 = 52.0;
const ROW_PADDING: f32 = 8.0;
const AVATAR_GAP: f32 = 10.0;
const SECTION_HEIGHT: f32 = 28.0;

const BADGE_MIN: f32 = 18.0;
const BADGE_DOT: f32 = 10.0;

/// "" for the plain dot, otherwise the count capped at "9+".
fn badge_label(count: usize) -> String {
    match count {
        0 => String::new(),
        1..=9 => count.to_string(),
        _ => "9+".into(),
    }
}

/// Lays out `text` on one row no wider than `max_width`, eliding with `…`.
/// All row text uses this single truncation path.
pub(crate) fn layout_truncated(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_width.max(0.0),
        max_rows: 1,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

pub(crate) struct ListRow<'a> {
    title: &'a str,
    timestamp: Option<&'a str>,
    preview: Option<&'a str>,
    selected: bool,
    unread: bool,
    badge: Option<usize>,
    avatar: Option<Avatar<'a>>,
    hover_menu: Option<HoverMenu<'a>>,
}

/// Contents of the row's trailing "More" popup menu.
type HoverMenu<'a> = Box<dyn FnMut(&mut Ui) + 'a>;

const MORE_BUTTON: f32 = 24.0;

impl<'a> ListRow<'a> {
    pub(crate) fn new(title: &'a str) -> Self {
        Self {
            title,
            timestamp: None,
            preview: None,
            selected: false,
            unread: false,
            badge: None,
            avatar: None,
            hover_menu: None,
        }
    }

    pub(crate) fn timestamp(mut self, timestamp: &'a str) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    pub(crate) fn preview(mut self, preview: &'a str) -> Self {
        self.preview = Some(preview);
        self
    }

    pub(crate) fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub(crate) fn unread(mut self, unread: bool) -> Self {
        self.unread = unread;
        self
    }

    /// An unread marker at the row's trailing edge: a count pill for 1 or
    /// more new messages, a plain dot for 0 (unread, count unknown). Only
    /// painted while the row is unread.
    pub(crate) fn badge(mut self, count: usize) -> Self {
        self.badge = Some(count);
        self
    }

    pub(crate) fn avatar(mut self, avatar: Avatar<'a>) -> Self {
        self.avatar = Some(avatar);
        self
    }

    /// Adds a trailing "More options" button that opens `menu`. The button is
    /// painted on hover, selection and keyboard focus (it stays focusable while
    /// hidden), and the same contents open from a secondary click on the row.
    pub(crate) fn hover_menu(mut self, menu: impl FnMut(&mut Ui) + 'a) -> Self {
        self.hover_menu = Some(Box::new(menu));
        self
    }

    pub(crate) fn show(mut self, ui: &mut Ui) -> Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                ui.is_enabled(),
                self.selected,
                self.title,
            )
        });
        let mut more_visible = false;
        let mut more_response = None;
        if self.hover_menu.is_some() {
            let more_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - ROW_PADDING - MORE_BUTTON, rect.top() + 4.0),
                egui::vec2(MORE_BUTTON, MORE_BUTTON),
            );
            let more_id = response.id.with("more_options");
            let focused = ui.memory(|memory| memory.has_focus(more_id));
            more_visible = response.hovered() || response.has_focus() || self.selected || focused;
            let sense = if more_visible {
                Sense::click()
            } else {
                Sense::focusable_noninteractive()
            };
            let more = ui.interact(more_rect, more_id, sense);
            more.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "More options")
            });
            more_visible |= more.hovered();
            more_response = Some((more_rect, more));
        }
        if let Some(menu) = self.hover_menu.as_mut() {
            if let Some((_, more)) = &more_response {
                egui::Popup::menu(more).show(|ui| menu(ui));
            }
            response.context_menu(|ui| menu(ui));
        }
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let rounding = CornerRadius::same(theme::radius::LG);
        if self.selected {
            ui.painter().rect_filled(rect, rounding, theme::surface());
        } else if response.hovered() {
            ui.painter()
                .rect_filled(rect, rounding, theme::surface_subtle());
        }

        let mut text_left = rect.left() + ROW_PADDING;
        if let Some(avatar) = self.avatar {
            let side = ROW_HEIGHT - 20.0;
            let avatar_rect = egui::Rect::from_min_size(
                egui::pos2(text_left, rect.center().y - side / 2.0),
                egui::vec2(side, side),
            );
            avatar.paint(ui, avatar_rect);
            text_left = avatar_rect.right() + AVATAR_GAP;
        }
        let text_right = rect.right() - ROW_PADDING;
        let mut text_width = (text_right - text_left).max(0.0);
        let mut preview_width_reserve = 0.0;
        if let Some(count) = self.badge.filter(|_| self.unread) {
            let label = badge_label(count);
            let galley = ui.painter().layout_no_wrap(
                label.clone(),
                theme::semibold(theme::font_size::CAPTION),
                theme::fg_on_brand(),
            );
            let (width, height) = if label.is_empty() {
                (BADGE_DOT, BADGE_DOT)
            } else {
                ((galley.size().x + 10.0).max(BADGE_MIN), BADGE_MIN)
            };
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(text_right - width, rect.top() + 29.0),
                egui::vec2(width, height),
            );
            ui.painter()
                .rect_filled(badge_rect, CornerRadius::same(255), theme::brand());
            if !label.is_empty() {
                ui.painter().galley(
                    egui::pos2(
                        badge_rect.center().x - galley.size().x / 2.0,
                        badge_rect.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    theme::fg_on_brand(),
                );
            }
            preview_width_reserve = width + ROW_PADDING;
        }

        let title_font = if self.unread || self.selected {
            theme::semibold(theme::font_size::BODY)
        } else {
            theme::regular(theme::font_size::BODY)
        };
        let mut title_width = text_width;
        if let Some(timestamp) = self.timestamp.filter(|_| !more_visible) {
            let stamp = layout_truncated(
                ui,
                timestamp,
                theme::regular(theme::font_size::CAPTION),
                theme::fg3(),
                text_width / 2.0,
            );
            title_width = (text_width - stamp.size().x - ROW_PADDING).max(0.0);
            let pos = egui::pos2(text_right - stamp.size().x, rect.top() + 9.0);
            ui.painter().galley(pos, stamp, theme::fg3());
        } else if more_visible {
            title_width = (text_width - MORE_BUTTON - ROW_PADDING).max(0.0);
        }
        let title = layout_truncated(ui, self.title, title_font, theme::fg1(), title_width);
        ui.painter()
            .galley(egui::pos2(text_left, rect.top() + 7.0), title, theme::fg1());

        if let Some(preview) = self.preview {
            let (font, color) = if self.unread {
                (theme::semibold(theme::font_size::CAPTION), theme::fg1())
            } else {
                (theme::regular(theme::font_size::CAPTION), theme::fg3())
            };
            text_width = (text_width - preview_width_reserve).max(0.0);
            let galley = layout_truncated(ui, preview, font, color, text_width);
            ui.painter()
                .galley(egui::pos2(text_left, rect.top() + 28.0), galley, color);
        }
        if more_visible && let Some((more_rect, more)) = more_response {
            if more.hovered() {
                ui.painter().rect_filled(
                    more_rect,
                    CornerRadius::same(theme::radius::SM),
                    theme::surface_hover(),
                );
            }
            let side = IconSize::S16.px();
            Icon::More.image(IconSize::S16).tint(theme::fg2()).paint_at(
                ui,
                egui::Rect::from_center_size(more_rect.center(), egui::vec2(side, side)),
            );
        }
        response
    }
}

/// Collapsible sidebar section header ("Favorites", "Chats", ...). Clicking
/// toggles `open`; the chevron points down when open and right when closed.
pub(crate) fn section_header(ui: &mut Ui, label: &str, open: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SECTION_HEIGHT),
        Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::CollapsingHeader, ui.is_enabled(), label)
    });
    if response.clicked() {
        *open = !*open;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                CornerRadius::same(theme::radius::SM),
                theme::surface_subtle(),
            );
        }
        let side = IconSize::S16.px();
        let chevron = if *open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        };
        chevron.image(IconSize::S16).tint(theme::fg3()).paint_at(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + ROW_PADDING, rect.center().y - side / 2.0),
                egui::vec2(side, side),
            ),
        );
        let galley = layout_truncated(
            ui,
            label,
            theme::semibold(theme::font_size::CAPTION),
            theme::fg2(),
            rect.width() - side - ROW_PADDING * 3.0,
        );
        ui.painter().galley(
            egui::pos2(
                rect.left() + ROW_PADDING * 2.0 + side,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            theme::fg2(),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::avatar::{AvatarSize, Presence};
    use crate::ui::testing::{run_ui, themed_context};

    #[test]
    fn long_text_is_elided_within_the_available_width() {
        let ctx = themed_context();
        run_ui(&ctx, |ui| {
            let long = "A very long conversation title that cannot possibly fit in a narrow row";
            let galley = layout_truncated(
                ui,
                long,
                theme::regular(theme::font_size::BODY),
                theme::fg1(),
                120.0,
            );
            assert!(galley.size().x <= 120.5, "width {}", galley.size().x);
            assert_eq!(galley.rows.len(), 1);
            let unbroken = layout_truncated(
                ui,
                "Supercalifragilisticexpialidocious_unbroken_token",
                theme::regular(theme::font_size::BODY),
                theme::fg1(),
                80.0,
            );
            assert!(unbroken.size().x <= 80.5, "width {}", unbroken.size().x);
        });
    }

    #[test]
    fn short_text_is_not_elided() {
        let ctx = themed_context();
        run_ui(&ctx, |ui| {
            let galley = layout_truncated(
                ui,
                "Hi",
                theme::regular(theme::font_size::BODY),
                theme::fg1(),
                200.0,
            );
            assert!(!galley.job.text.contains('…'));
        });
    }

    #[test]
    fn rows_and_headers_render() {
        let ctx = themed_context();
        let mut open = true;
        run_ui(&ctx, |ui| {
            section_header(ui, "Favorites", &mut open);
            for (selected, unread) in [(false, false), (true, false), (false, true)] {
                let response = ListRow::new("Ada Lovelace with a rather long display name")
                    .timestamp("10:42 AM")
                    .preview("You: see you at the standup tomorrow morning, bring the notes")
                    .selected(selected)
                    .unread(unread)
                    .avatar(
                        Avatar::new("Ada Lovelace", AvatarSize::Md32).presence(Presence::Available),
                    )
                    .show(ui);
                assert_eq!(response.rect.height(), ROW_HEIGHT);
            }
            ListRow::new("Bare row").show(ui);
        });
        assert!(open);
    }
}

#[cfg(test)]
mod badge_tests {
    use super::badge_label;

    #[test]
    fn the_badge_is_a_dot_a_count_or_capped() {
        assert_eq!(badge_label(0), "");
        assert_eq!(badge_label(1), "1");
        assert_eq!(badge_label(9), "9");
        assert_eq!(badge_label(10), "9+");
        assert_eq!(badge_label(500), "9+");
    }
}
