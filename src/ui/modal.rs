use super::theme;
use eframe::egui::{self, RichText, Stroke};

/// Shows a consistent, input-blocking dialog for local workspace actions.
///
/// `egui::Modal` owns the foreground layer and prevents clicks from reaching
/// the surrounding workspace while a dialog is open.
pub(crate) fn show<T>(
    ctx: &egui::Context,
    id: impl Into<egui::Id>,
    width: f32,
    content: impl FnOnce(&mut egui::Ui) -> T,
) -> egui::ModalResponse<T> {
    egui::Modal::new(id.into())
        .frame(
            egui::Frame::new()
                .fill(theme::surface())
                .stroke(Stroke::new(1.0_f32, theme::brand_stroke_subtle()))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(22, 18))
                .shadow(egui::Shadow::NONE),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            content(ui)
        })
}

pub(crate) fn heading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        RichText::new(title)
            .font(theme::semibold(theme::font_size::SUBTITLE))
            .color(theme::fg1()),
    );
}
