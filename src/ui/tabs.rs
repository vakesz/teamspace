//! Teams-style header tabs: semibold selected tab with a rounded brand underline.

use super::theme;
use eframe::egui::{self, CornerRadius, Sense, Ui};

const TAB_HEIGHT: f32 = 36.0;
const TAB_PADDING_X: f32 = 12.0;
const UNDERLINE_HEIGHT: f32 = 3.0;

/// Resolves the selection after a frame: a click on a valid tab wins,
/// otherwise the current selection is kept (clamped into range).
pub(crate) fn next_selection(selected: usize, clicked: Option<usize>, len: usize) -> usize {
    match clicked {
        Some(index) if index < len => index,
        _ => selected.min(len.saturating_sub(1)),
    }
}

/// Draws the tab strip and returns the (possibly updated) selected index.
pub(crate) fn tab_strip(ui: &mut Ui, labels: &[&str], selected: usize) -> usize {
    let clicked = tab_strip_clicked(ui, labels, selected);
    next_selection(selected, clicked, labels.len())
}

/// Draws the tab strip and returns the index of the tab clicked this frame, if
/// any. `selected` may be out of range when no listed tab is active.
pub(crate) fn tab_strip_clicked(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (index, label) in labels.iter().enumerate() {
            let is_selected = index == selected;
            let font = if is_selected {
                theme::semibold(theme::font_size::BODY)
            } else {
                theme::regular(theme::font_size::BODY)
            };
            let color = if is_selected {
                theme::fg1()
            } else {
                theme::fg2()
            };
            let galley = ui
                .painter()
                .layout_no_wrap((*label).to_owned(), font, color);
            let size = egui::vec2(galley.size().x + TAB_PADDING_X * 2.0, TAB_HEIGHT);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    ui.is_enabled(),
                    is_selected,
                    *label,
                )
            });
            if response.clicked() {
                clicked = Some(index);
            }
            if !ui.is_rect_visible(rect) {
                continue;
            }
            if response.hovered() && !is_selected {
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(theme::radius::SM),
                    theme::surface_hover(),
                );
            }
            let text_pos = egui::pos2(
                rect.left() + TAB_PADDING_X,
                rect.center().y - galley.size().y / 2.0 - 1.0,
            );
            ui.painter().galley(text_pos, galley.clone(), color);
            if is_selected {
                let underline = egui::Rect::from_min_size(
                    egui::pos2(
                        rect.left() + TAB_PADDING_X,
                        rect.bottom() - UNDERLINE_HEIGHT,
                    ),
                    egui::vec2(galley.size().x, UNDERLINE_HEIGHT),
                );
                ui.painter()
                    .rect_filled(underline, CornerRadius::same(2), theme::brand_fg());
            }
        }
    });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_selects_the_clicked_tab() {
        assert_eq!(next_selection(0, Some(2), 3), 2);
    }

    #[test]
    fn no_click_keeps_the_selection() {
        assert_eq!(next_selection(1, None, 3), 1);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        assert_eq!(next_selection(5, None, 3), 2);
        assert_eq!(next_selection(1, Some(9), 3), 1);
        assert_eq!(next_selection(4, None, 0), 0);
    }

    #[test]
    fn render_smoke_returns_the_selection() {
        let ctx = crate::ui::testing::themed_context();
        let mut result = usize::MAX;
        crate::ui::testing::run_ui(&ctx, |ui| {
            result = tab_strip(ui, &["Posts", "Files", "Notes"], 1);
        });
        assert_eq!(result, 1);
    }
}
