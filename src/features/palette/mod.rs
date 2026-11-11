//! Ctrl/Cmd+K command palette: a fuzzy-filtered list of app-level actions.

use crate::shared::model::Section;
use crate::ui::shortcuts::{primary_chord, take_escape};
use crate::ui::theme;
use eframe::egui::{self, Color32, RichText, Stroke};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    Navigate(Section),
    NewChat,
    NewMeeting,
    ToggleSidebar,
    Refresh,
    SignOut,
    ShowShortcuts,
    OpenSettings,
}

/// What the shell can currently do, so the palette lists only runnable
/// commands.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaletteContext {
    pub(crate) refresh_available: bool,
    pub(crate) sidebar_collapsed: bool,
}

struct Command {
    action: PaletteAction,
    title: String,
    keywords: &'static str,
    hint: Option<String>,
}

const NAV: [(Section, &str, &str, &str); 6] = [
    (Section::Activity, "Activity", "1", "go open notifications"),
    (Section::Chat, "Chat", "2", "go open messages"),
    (Section::Teams, "Teams", "3", "go open channels"),
    (
        Section::Calendar,
        "Calendar",
        "4",
        "go open meetings schedule",
    ),
    (Section::Calls, "Calls", "5", "go open phone"),
    (Section::Files, "Files", "6", "go open documents"),
];

fn commands(context: PaletteContext) -> Vec<Command> {
    let mut commands: Vec<Command> = NAV
        .iter()
        .map(|(section, name, key, keywords)| Command {
            action: PaletteAction::Navigate(*section),
            title: format!("Go to {name}"),
            keywords,
            hint: Some(primary_chord(key)),
        })
        .collect();
    let mut push = |action, title: &str, keywords, hint| {
        commands.push(Command {
            action,
            title: title.into(),
            keywords,
            hint,
        });
    };
    push(
        PaletteAction::NewChat,
        "New chat",
        "message compose start",
        None,
    );
    push(
        PaletteAction::NewMeeting,
        "New meeting",
        "event schedule calendar",
        None,
    );
    push(
        PaletteAction::ToggleSidebar,
        if context.sidebar_collapsed {
            "Show sidebar"
        } else {
            "Hide sidebar"
        },
        "toggle panel list",
        None,
    );
    if context.refresh_available {
        push(
            PaletteAction::Refresh,
            "Refresh Microsoft 365 data",
            "reload sync update",
            Some("F5".into()),
        );
    }
    push(
        PaletteAction::OpenSettings,
        "Open settings",
        "preferences options theme appearance notifications privacy",
        Some(primary_chord(",")),
    );
    push(
        PaletteAction::ShowShortcuts,
        "Keyboard shortcuts",
        "help keys",
        None,
    );
    push(PaletteAction::SignOut, "Sign out", "log out account", None);
    commands
}

/// Subsequence score of `query` in `text`, ignoring case; `None` when the
/// query's characters do not all appear in order. Word starts and contiguous
/// runs score higher, and earlier matches beat later ones.
pub(crate) fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let mut wanted = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .peekable();
    wanted.peek()?;
    let mut score = 0;
    let mut previous: Option<char> = None;
    let mut last_match: Option<usize> = None;
    let mut first_match: Option<usize> = None;
    for (index, ch) in text.chars().flat_map(char::to_lowercase).enumerate() {
        if let Some(&target) = wanted.peek()
            && ch == target
        {
            score += 10;
            if last_match.is_some_and(|last| last + 1 == index) {
                score += 15;
            }
            if previous.is_none_or(|p| !p.is_alphanumeric()) {
                score += 20;
            }
            first_match.get_or_insert(index);
            last_match = Some(index);
            wanted.next();
        }
        previous = Some(ch);
    }
    if wanted.peek().is_some() {
        return None;
    }
    Some(score - i32::try_from(first_match.unwrap_or(0)).unwrap_or(0))
}

/// Indexes of the commands matching `query`, best first. An empty query lists
/// every command in registry order.
fn filtered(commands: &[Command], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..commands.len()).collect();
    }
    let mut scored: Vec<(usize, i32)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let title = fuzzy_score(query, &command.title);
            let keywords = fuzzy_score(query, command.keywords).map(|score| score - 50);
            title.max(keywords).map(|score| (index, score))
        })
        .collect();
    // Stable: ties keep registry order.
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[derive(Default)]
pub(crate) struct PaletteState {
    open: bool,
    query: String,
    selected: usize,
    focus_requested: bool,
}

impl PaletteState {
    pub(crate) const fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.focus_requested = true;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
    }

    /// Draws the palette and returns the command to run, if one was chosen.
    pub(crate) fn draw(
        &mut self,
        ctx: &egui::Context,
        context: PaletteContext,
    ) -> Option<PaletteAction> {
        if !self.open {
            return None;
        }
        if take_escape(ctx) {
            self.close();
            return None;
        }
        let all = commands(context);
        let matches = filtered(&all, &self.query);
        let (down, up, enter) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Enter),
            )
        });
        self.selected = step(self.selected, matches.len(), down, up);
        let mut chosen = (enter)
            .then(|| matches.get(self.selected).map(|&i| all[i].action))
            .flatten();

        egui::Area::new(egui::Id::new("command_palette"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 64.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(theme::surface())
                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(480.0);
                        let input = ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .id(egui::Id::new("command_palette_input"))
                                .hint_text("Type a command")
                                .desired_width(f32::INFINITY),
                        );
                        if self.focus_requested {
                            input.request_focus();
                            self.focus_requested = false;
                        }
                        ui.add_space(theme::space::XS);
                        if matches.is_empty() {
                            ui.label(
                                RichText::new("No matching commands.")
                                    .font(theme::regular(theme::font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                        }
                        for (row, &index) in matches.iter().enumerate() {
                            let command = &all[index];
                            let selected = row == self.selected;
                            let response = ui.add_sized(
                                [ui.available_width(), 32.0],
                                egui::Button::new(
                                    RichText::new(&command.title)
                                        .font(theme::regular(theme::font_size::CAPTION))
                                        .color(theme::fg1()),
                                )
                                .fill(if selected {
                                    theme::brand_tint_subtle()
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .stroke(Stroke::NONE),
                            );
                            if let Some(hint) = &command.hint {
                                ui.painter().text(
                                    response.rect.right_center() - egui::vec2(8.0, 0.0),
                                    egui::Align2::RIGHT_CENTER,
                                    hint,
                                    theme::regular(theme::font_size::MICRO),
                                    theme::fg3(),
                                );
                            }
                            if selected {
                                ui.scroll_to_rect(response.rect, None);
                            }
                            if response.clicked() {
                                chosen = Some(command.action);
                            }
                        }
                        ui.add_space(theme::space::XS);
                        ui.label(
                            RichText::new("Esc closes · Up/Down moves · Enter runs")
                                .font(theme::regular(theme::font_size::MICRO))
                                .color(theme::fg3()),
                        );
                    });
            });
        if chosen.is_some() {
            self.close();
        }
        chosen
    }
}

fn step(current: usize, count: usize, down: bool, up: bool) -> usize {
    if count == 0 {
        return 0;
    }
    let mut selected = current.min(count - 1);
    if down {
        selected = (selected + 1) % count;
    }
    if up {
        selected = selected.checked_sub(1).unwrap_or(count - 1);
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::{PaletteAction, PaletteContext, commands, filtered, fuzzy_score, step};
    use crate::shared::model::Section;

    #[test]
    fn fuzzy_requires_an_ordered_subsequence() {
        assert!(fuzzy_score("gcl", "Go to Calendar").is_some());
        assert!(fuzzy_score("lcg", "Go to Calendar").is_none());
        assert!(fuzzy_score("", "anything").is_none());
        assert!(fuzzy_score("CAL", "calendar").is_some());
        assert!(fuzzy_score("ÉA", "éa").is_some());
    }

    #[test]
    fn contiguous_and_word_start_matches_rank_higher() {
        let contiguous = fuzzy_score("cal", "Go to Calendar").unwrap();
        let scattered = fuzzy_score("cal", "Collapse all").unwrap();
        assert!(contiguous > scattered);
    }

    #[test]
    fn filtering_ranks_best_first_and_drops_non_matches() {
        let all = commands(PaletteContext::default());
        let hits = filtered(&all, "cal");
        assert_eq!(
            all[hits[0]].action,
            PaletteAction::Navigate(Section::Calendar)
        );
        assert!(filtered(&all, "zzzz").is_empty());
        assert_eq!(filtered(&all, "  ").len(), all.len());
    }

    #[test]
    fn availability_gates_commands() {
        let none = commands(PaletteContext::default());
        assert!(none.iter().all(|c| c.action != PaletteAction::Refresh));
        let some = commands(PaletteContext {
            refresh_available: true,
            sidebar_collapsed: true,
        });
        assert!(some.iter().any(|c| c.action == PaletteAction::Refresh));
        assert!(some.iter().any(|c| c.title == "Show sidebar"));
    }

    fn key_frame(ctx: &eframe::egui::Context, key: eframe::egui::Key) {
        ctx.begin_pass(eframe::egui::RawInput {
            events: vec![eframe::egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: eframe::egui::Modifiers::NONE,
            }],
            ..Default::default()
        });
    }

    #[test]
    fn typing_then_enter_runs_the_best_match_and_closes() {
        let ctx = crate::ui::testing::themed_context();
        let mut palette = super::PaletteState::default();
        palette.open();
        palette.query.push_str("cal");
        key_frame(&ctx, eframe::egui::Key::Enter);
        let action = palette.draw(&ctx, PaletteContext::default());
        let _ = ctx.end_pass();
        assert_eq!(action, Some(PaletteAction::Navigate(Section::Calendar)));
        assert!(!palette.is_open());
    }

    #[test]
    fn escape_closes_without_running_anything() {
        let ctx = crate::ui::testing::themed_context();
        let mut palette = super::PaletteState::default();
        palette.open();
        key_frame(&ctx, eframe::egui::Key::Escape);
        assert_eq!(palette.draw(&ctx, PaletteContext::default()), None);
        let _ = ctx.end_pass();
        assert!(!palette.is_open());
    }

    #[test]
    fn selection_wraps() {
        assert_eq!(step(0, 3, false, true), 2);
        assert_eq!(step(2, 3, true, false), 0);
        assert_eq!(step(5, 2, false, false), 1);
        assert_eq!(step(1, 0, true, false), 0);
    }
}
