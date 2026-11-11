//! Renders formatted message runs as one wrapped, selectable-looking text
//! block, with clickable links.
//!
//! Every colour and font comes from `ui::theme`. A link is never opened
//! here: a click is reported as the index of the link run, and the owner
//! decides what to do (the app revalidates at its browser boundary).

use super::theme;
use crate::integrations::{Inline, Marks, RichRun};
use eframe::egui::{
    self, Color32, CursorIcon, Sense, Stroke,
    text::{LayoutJob, TextFormat},
};
use std::borrow::Cow;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Link,
    Mention,
}

fn format_for(marks: Marks, kind: Kind) -> TextFormat {
    let link = kind == Kind::Link;
    let marks = Marks {
        bold: marks.bold || kind == Kind::Mention,
        ..marks
    };
    let size = theme::font_size::BODY;
    let font_id = if marks.code {
        theme::mono(size)
    } else if marks.bold {
        theme::semibold(size)
    } else {
        theme::regular(size)
    };
    TextFormat {
        font_id,
        color: if kind == Kind::Plain {
            theme::fg1()
        } else {
            theme::brand_fg()
        },
        background: if marks.code {
            theme::surface_subtle()
        } else {
            Color32::TRANSPARENT
        },
        italics: marks.italic,
        underline: if link {
            Stroke::new(1.0_f32, theme::brand_fg())
        } else {
            Stroke::NONE
        },
        ..TextFormat::default()
    }
}

fn run_parts(run: &RichRun) -> (Cow<'_, str>, Marks, Kind) {
    match run {
        Inline::Text { text, marks } => (Cow::Borrowed(text), *marks, Kind::Plain),
        Inline::Link { text, marks, .. } => (Cow::Borrowed(text), *marks, Kind::Link),
        Inline::Mention { display, .. } => (
            Cow::Owned(format!("@{display}")),
            Marks::default(),
            Kind::Mention,
        ),
    }
}

/// The wrapped layout of `runs`, one section per run.
pub(crate) fn layout_job(runs: &[RichRun], wrap_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for run in runs {
        let (text, marks, kind) = run_parts(run);
        job.append(&text, 0.0, format_for(marks, kind));
    }
    job
}

/// The index of the run containing the `char_index`-th character of the
/// concatenated text, or `None` past the end.
pub(crate) fn run_at_char(runs: &[RichRun], char_index: usize) -> Option<usize> {
    let mut start = 0;
    for (index, run) in runs.iter().enumerate() {
        let end = start + run_parts(run).0.chars().count();
        if char_index < end {
            return Some(index);
        }
        start = end;
    }
    None
}

fn link_run_at(runs: &[RichRun], char_index: usize) -> Option<usize> {
    run_at_char(runs, char_index).filter(|index| matches!(runs[*index], Inline::Link { .. }))
}

/// Draws `runs` and returns the index of a link run that was clicked.
pub(crate) fn draw_rich(ui: &mut egui::Ui, runs: &[RichRun]) -> Option<usize> {
    let galley = ui
        .painter()
        .layout_job(layout_job(runs, ui.available_width()));
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::click());
    if ui.is_rect_visible(rect) {
        ui.painter().galley(rect.min, galley.clone(), theme::fg1());
    }
    let link_at = |pos: egui::Pos2| {
        if !rect.contains(pos) {
            return None;
        }
        let cursor = galley.cursor_from_pos(pos - rect.min);
        link_run_at(runs, cursor.index)
    };
    if let Some(index) = response.hover_pos().and_then(link_at) {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        if let Inline::Link { url, .. } = &runs[index] {
            let host = url.host().unwrap_or_default();
            response
                .clone()
                .on_hover_text(format!("Opens in your browser: {host}"));
        }
    }
    if response.clicked() {
        return response.interact_pointer_pos().and_then(link_at);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::SafeHttpsUrl;

    fn runs() -> Vec<RichRun> {
        vec![
            Inline::Text {
                text: "ab ".into(),
                marks: Marks::default(),
            },
            Inline::Link {
                text: "é日".into(),
                marks: Marks::default(),
                url: SafeHttpsUrl::parse("https://example.com").unwrap(),
            },
            Inline::Text {
                text: " end".into(),
                marks: Marks {
                    bold: true,
                    italic: true,
                    code: false,
                },
            },
        ]
    }

    #[test]
    fn characters_map_to_their_runs() {
        let runs = runs();
        assert_eq!(run_at_char(&runs, 0), Some(0));
        assert_eq!(run_at_char(&runs, 2), Some(0));
        assert_eq!(run_at_char(&runs, 3), Some(1));
        assert_eq!(run_at_char(&runs, 4), Some(1));
        assert_eq!(run_at_char(&runs, 5), Some(2));
        assert_eq!(run_at_char(&runs, 8), Some(2));
        assert_eq!(run_at_char(&runs, 9), None);
        assert_eq!(link_run_at(&runs, 4), Some(1));
        assert_eq!(link_run_at(&runs, 0), None);
    }

    #[test]
    fn layout_job_uses_theme_tokens_only() {
        let job = layout_job(&runs(), 300.0);
        assert_eq!(job.sections.len(), 3);
        assert_eq!(job.sections[0].format.color, theme::fg1());
        assert_eq!(job.sections[1].format.color, theme::brand_fg());
        assert_eq!(job.sections[1].format.underline.color, theme::brand_fg());
        assert_eq!(
            job.sections[2].format.font_id,
            theme::semibold(theme::font_size::BODY)
        );
        assert!(job.sections[2].format.italics);
        assert_eq!(job.text, "ab é日 end");
        let code = layout_job(
            &[Inline::Text {
                text: "x".into(),
                marks: Marks {
                    bold: false,
                    italic: false,
                    code: true,
                },
            }],
            100.0,
        );
        assert_eq!(
            code.sections[0].format.font_id,
            theme::mono(theme::font_size::BODY)
        );
        assert_eq!(code.sections[0].format.background, theme::surface_subtle());
    }
}
