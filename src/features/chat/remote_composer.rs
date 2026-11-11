//! The formatting toolbar of the remote (Microsoft 365) chat composer.
//!
//! The text field itself stays in `message_pane.rs`; this module owns its
//! stable identity and the toolbar that edits its markdown-lite source.

use super::composer_markup::{Wrap, wrap_selection};
use crate::integrations::{ComposedBody, RichRun};
use crate::ui::{rich_text, theme};
use eframe::egui::{self, RichText, text::CCursorRange};
use std::sync::Arc;

/// Composer hint lines, in one place. They name no keyboard shortcut: when
/// shortcuts arrive their hint must come from the shared modifier label.
pub(super) const HINT_SEND: &str = "Type @ to mention · use **bold**, _italic_, `code` or [text](https://link) · choose Normal, High, or Urgent priority";
pub(super) const HINT_EDIT: &str =
    "Edits keep **bold**, _italic_, `code` and links · saved for everyone in the chat";
pub(super) const HINT_QUOTE: &str = "Quoted replies are plain text and Normal priority";

/// Stable identity of the remote composer text field, so other surfaces can
/// return focus to it.
pub(crate) fn remote_composer_id() -> egui::Id {
    egui::Id::new("remote_chat_composer_input")
}

/// Toolbar labels and tooltips live here, in one place. No keyboard shortcut
/// is bound: Ctrl/Cmd+K is reserved globally, and any future link shortcut
/// must be Ctrl/Cmd+Shift+K with its hint built from the shared modifier
/// label rather than a hard-coded key name.
const TOOLBAR: [(Wrap, &str, &str); 4] = [
    (Wrap::Bold, "B", "Bold"),
    (Wrap::Italic, "I", "Italic"),
    (Wrap::Code, "<>", "Code"),
    (Wrap::Link, "Link", "Link"),
];

/// Draws the toolbar. Returns whether `draft` changed. The text field's
/// selection is read from and written back to egui's stored edit state.
pub(super) fn draw_format_toolbar(ui: &mut egui::Ui, draft: &mut String) -> bool {
    let id = remote_composer_id();
    let mut changed = false;
    ui.horizontal(|ui| {
        for (wrap, label, tooltip) in TOOLBAR {
            let button = egui::Button::new(
                RichText::new(label)
                    .font(theme::semibold(theme::font_size::CAPTION))
                    .color(theme::fg2()),
            )
            .small()
            .frame(false);
            if ui.add(button).on_hover_text(tooltip).clicked() {
                let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                let (start, end) = state.cursor.char_range().map_or_else(
                    || {
                        let end = draft.chars().count();
                        (end, end)
                    },
                    |range| (range.primary.index, range.secondary.index),
                );
                let (text, new_start, new_end) = wrap_selection(draft, start, end, wrap);
                *draft = text;
                state.cursor.set_char_range(Some(CCursorRange::two(
                    egui::text::CCursor::new(new_start),
                    egui::text::CCursor::new(new_end),
                )));
                state.store(ui.ctx(), id);
                ui.memory_mut(|memory| memory.request_focus(id));
                changed = true;
            }
        }
    });
    changed
}

/// The parsed preview of the last draft text. The draft is re-parsed only
/// when its text changes, not on every frame.
#[derive(Clone, Default)]
struct PreviewCache {
    source: String,
    runs: Option<Arc<[RichRun]>>,
    #[cfg(test)]
    parses: usize,
}

impl PreviewCache {
    /// Runs to preview, or `None` for a draft with no formatting.
    fn runs_for(&mut self, draft: &str) -> Option<Arc<[RichRun]>> {
        if self.source != draft {
            draft.clone_into(&mut self.source);
            let body = ComposedBody::from_markdown_lite(draft);
            self.runs = (!body.is_plain()).then(|| Arc::from(body.to_runs()));
            #[cfg(test)]
            {
                self.parses += 1;
            }
        }
        self.runs.clone()
    }
}

fn preview_cache_id() -> egui::Id {
    egui::Id::new("remote_chat_composer_preview")
}

/// Shows how the draft will look once sent, only when it has formatting.
pub(super) fn draw_preview(ui: &mut egui::Ui, draft: &str) {
    let runs = ui.data_mut(|data| {
        data.get_temp_mut_or_default::<PreviewCache>(preview_cache_id())
            .runs_for(draft)
    });
    let Some(runs) = runs else {
        return;
    };
    egui::Frame::new()
        .fill(theme::surface_subtle())
        .corner_radius(egui::CornerRadius::same(theme::radius::SM))
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.label(
                RichText::new("Preview")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
            // A preview link is never opened from here.
            let _ = rich_text::draw_rich(ui, &runs);
        });
    ui.add_space(theme::space::XS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_parses_only_when_the_draft_text_changes() {
        let mut cache = PreviewCache::default();
        assert!(cache.runs_for("plain").is_none());
        assert_eq!(cache.parses, 1);
        for _ in 0..5 {
            assert!(cache.runs_for("plain").is_none());
        }
        assert_eq!(cache.parses, 1);
        let runs = cache.runs_for("a **b**").expect("formatted draft previews");
        assert_eq!(cache.parses, 2);
        assert_eq!(runs.len(), 2);
        for _ in 0..5 {
            let _ = cache.runs_for("a **b**");
        }
        assert_eq!(cache.parses, 2);
        assert!(cache.runs_for("a **b").is_none());
        assert_eq!(cache.parses, 3);
    }
}
