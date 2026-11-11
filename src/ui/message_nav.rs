//! Keyboard navigation across a list of messages or posts.
//!
//! Each row registers a focus target while it is drawn. After the list is
//! drawn, [`MessageNav::handle`] moves focus between the rows, opens a row's
//! action toolbar, and hands focus back to the composer. Navigation only acts
//! while a row already holds focus (or on an explicit entry), so typing in the
//! composer, a mention picker, or any text field is never intercepted.

use crate::ui::shortcuts::take_escape;
use crate::ui::theme::{self, radius};
use eframe::egui::{self, CornerRadius, EventFilter, Id, Key, Modifiers, Rect, Sense, Stroke};

/// A row's keyboard-focus target and the controls it can hand focus to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NavItem {
    pub(crate) id: Id,
    pub(crate) rect: Rect,
    /// Control that receives focus on Enter or Space, when the row has an
    /// action toolbar.
    pub(crate) activate: Option<Id>,
    /// Further toolbar controls; Esc from any of them returns to the row.
    pub(crate) secondary: Option<Id>,
}

/// Registers `rect` as a focusable, non-clickable target so Tab and the
/// navigation keys can reach it, and draws a focus ring around it.
pub(crate) fn focus_target(ui: &egui::Ui, rect: Rect, id: Id, label: &str) -> bool {
    let response = ui.interact(rect, id, Sense::focusable_noninteractive());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, ui.is_enabled(), label));
    let focused = response.has_focus();
    if focused {
        // Keep Up/Down and Escape for the list instead of egui's own
        // directional focus movement.
        ui.memory_mut(|memory| memory.set_focus_lock_filter(id, list_filter()));
        ui.painter().rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(radius::MD),
            Stroke::new(2.0_f32, theme::brand_fg()),
            egui::StrokeKind::Outside,
        );
    }
    focused
}

const fn list_filter() -> EventFilter {
    EventFilter {
        tab: false,
        horizontal_arrows: false,
        vertical_arrows: true,
        escape: true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Previous,
    Next,
}

/// The row to focus after `step`. Movement stops at both ends; with nothing
/// focused, Previous enters at the newest row and Next at the oldest.
pub(crate) fn next_index(current: Option<usize>, count: usize, step: Step) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (current, step) {
        (None, Step::Previous) => count - 1,
        (None, Step::Next) => 0,
        (Some(index), Step::Previous) => index.saturating_sub(1).min(count - 1),
        (Some(index), Step::Next) => (index + 1).min(count - 1),
    })
}

/// What the list should do after navigation was handled this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavOutcome {
    None,
    /// Enter or Space on a row that has no toolbar to open.
    Activated(usize),
}

/// How navigation may begin when no row has focus yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// Only a row that already has focus (from Tab or a click) navigates.
    Never,
    /// Up Arrow alone, from an empty composer. Letters stay text; Down is
    /// not an entry because the composer has nothing below it.
    FromComposer,
    /// Nothing holds focus: K and J enter the list (at the newest and oldest
    /// row). Arrow keys are left to egui's own focus movement, which would
    /// override a request made in the same frame.
    Anywhere,
}

#[derive(Default)]
pub(crate) struct MessageNav {
    items: Vec<NavItem>,
}

impl MessageNav {
    pub(crate) fn push(&mut self, item: NavItem) {
        self.items.push(item);
    }

    /// Applies this frame's navigation keys. Call inside the list's scroll
    /// area after every row was pushed. `composer` receives focus on Esc.
    /// `entry` says how the list may be entered from outside it.
    pub(crate) fn handle(&self, ui: &mut egui::Ui, composer: Id, entry: Entry) -> NavOutcome {
        let ctx = ui.ctx().clone();
        let has_focus = |id: Id| ctx.memory(|memory| memory.has_focus(id));
        let focused = self.items.iter().position(|item| has_focus(item.id));
        let in_toolbar = self.items.iter().position(|item| {
            item.activate.is_some_and(has_focus) || item.secondary.is_some_and(has_focus)
        });
        if let Some(position) = in_toolbar {
            let item = self.items[position];
            ctx.memory_mut(|memory| {
                for id in [item.activate, item.secondary].into_iter().flatten() {
                    if memory.has_focus(id) {
                        memory.set_focus_lock_filter(id, list_filter());
                    }
                }
            });
            if take_escape(&ctx) {
                ctx.memory_mut(|memory| {
                    memory.request_focus(item.id);
                    memory.set_focus_lock_filter(item.id, list_filter());
                });
            }
            return NavOutcome::None;
        }
        if self.items.is_empty() || (focused.is_none() && entry == Entry::Never) {
            return NavOutcome::None;
        }
        let inside = focused.is_some();
        if entry == Entry::FromComposer && has_focus(composer) {
            // The field's own filter lets egui move focus on Up at the end of
            // the frame, which would override the row requested below.
            ctx.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    composer,
                    EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
        }

        let (step, activate) = ctx.input_mut(|input| {
            if input.modifiers.command || input.modifiers.alt || input.modifiers.shift {
                return (None, false);
            }
            let arrows = inside || entry == Entry::FromComposer;
            let letters = inside || entry == Entry::Anywhere;
            let previous = (arrows && input.consume_key(Modifiers::NONE, Key::ArrowUp))
                || (letters && input.consume_key(Modifiers::NONE, Key::K));
            let next = (inside && input.consume_key(Modifiers::NONE, Key::ArrowDown))
                || (letters && input.consume_key(Modifiers::NONE, Key::J));
            let step = match (previous, next) {
                (true, _) => Some(Step::Previous),
                (_, true) => Some(Step::Next),
                _ => None,
            };
            let activate = inside
                && (input.consume_key(Modifiers::NONE, Key::Enter)
                    || input.consume_key(Modifiers::NONE, Key::Space));
            (step, activate)
        });

        if let Some(step) = step {
            if let Some(index) = next_index(focused, self.items.len(), step) {
                let target = self.items[index];
                ctx.memory_mut(|memory| {
                    memory.request_focus(target.id);
                    memory.set_focus_lock_filter(target.id, list_filter());
                });
                ui.scroll_to_rect(target.rect, Some(egui::Align::Center));
            }
            return NavOutcome::None;
        }
        let Some(position) = focused else {
            return NavOutcome::None;
        };
        if activate {
            return match self.items[position].activate {
                Some(toolbar) => {
                    ctx.memory_mut(|memory| {
                        memory.request_focus(toolbar);
                        memory.set_focus_lock_filter(toolbar, list_filter());
                    });
                    NavOutcome::None
                }
                None => NavOutcome::Activated(position),
            };
        }
        if take_escape(&ctx) {
            ctx.memory_mut(|memory| memory.request_focus(composer));
        }
        NavOutcome::None
    }
}

#[cfg(test)]
mod tests {
    use super::{Step, next_index};

    #[test]
    fn movement_clamps_at_both_ends() {
        assert_eq!(next_index(Some(0), 3, Step::Previous), Some(0));
        assert_eq!(next_index(Some(2), 3, Step::Next), Some(2));
        assert_eq!(next_index(Some(1), 3, Step::Next), Some(2));
        assert_eq!(next_index(Some(1), 3, Step::Previous), Some(0));
    }

    #[test]
    fn entry_picks_the_newest_going_up_and_the_oldest_going_down() {
        assert_eq!(next_index(None, 4, Step::Previous), Some(3));
        assert_eq!(next_index(None, 4, Step::Next), Some(0));
        assert_eq!(next_index(None, 0, Step::Next), None);
        // A stale index past the end (the list shrank) is pulled back in.
        assert_eq!(next_index(Some(9), 3, Step::Previous), Some(2));
    }
}
