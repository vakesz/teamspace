//! Accessibility helpers for hand-rolled clickable regions.

use eframe::egui;

/// Makes `response` clickable and exposes `label` to assistive technology as
/// a button, for rows and cards built from frames or painted rectangles that
/// otherwise have no accessible name.
pub(crate) fn clickable(ui: &egui::Ui, response: egui::Response, label: &str) -> egui::Response {
    let response = response.interact(egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    response
}
