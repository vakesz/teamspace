//! Shared helpers for component render smoke tests.

use super::theme;
use eframe::egui;

pub(crate) fn themed_context() -> egui::Context {
    let ctx = egui::Context::default();
    egui_extras::install_image_loaders(&ctx);
    theme::apply(&ctx);
    ctx
}

/// Runs one egui pass with `add` laid out in a central panel.
pub(crate) fn run_ui(ctx: &egui::Context, add: impl FnOnce(&mut egui::Ui)) {
    ctx.begin_pass(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(400.0, 600.0),
        )),
        ..Default::default()
    });
    egui::CentralPanel::default().show(ctx, add);
    let _ = ctx.end_pass();
}
