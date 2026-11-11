//! The `@` picker of the remote (Microsoft 365) chat composer.
//!
//! Typing `@` after a space (or at the start) opens a short list of the chat's
//! members, taken from the People roster the feature already holds. Picking
//! one inserts `@Name ` and remembers the member's opaque handle; the draft
//! only becomes a mention at send, when each intact `@Name` is matched. Until
//! a roster has been read there is nothing to offer, so the picker says how
//! to load one instead of reading it on its own (reading it also asks for
//! presence, which belongs to an explicit People open).

use super::remote::RemoteConversation;
use super::remote_composer::remote_composer_id;
use crate::integrations::DraftMention;
use crate::ui::theme;
use eframe::egui::{
    self, Align2, CornerRadius, Frame, Key, Margin, Modifiers, Order, RichText, Stroke,
    text::{CCursor, CCursorRange},
};

/// Candidates shown at once.
const MAX_SHOWN: usize = 6;
/// How far back from the cursor an `@` still counts as the one being typed.
const MAX_QUERY_CHARS: usize = 40;

/// The `@query` being typed: where its `@` is and the text after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Trigger {
    /// Char index of the `@`.
    pub(super) start: usize,
    pub(super) query: String,
}

/// The `@query` that ends at `cursor` (a char index), if one is being typed:
/// an `@` at the start or after a non-alphanumeric character, with no line
/// break between it and the cursor.
pub(super) fn trigger(draft: &str, cursor: usize) -> Option<Trigger> {
    let chars: Vec<char> = draft.chars().collect();
    let cursor = cursor.min(chars.len());
    let lowest = cursor.saturating_sub(MAX_QUERY_CHARS);
    let mut index = cursor;
    while index > lowest {
        index -= 1;
        match chars[index] {
            '\n' => return None,
            '@' => {
                let boundary = index == 0 || !chars[index - 1].is_alphanumeric();
                return boundary.then(|| Trigger {
                    start: index,
                    query: chars[index + 1..cursor].iter().collect(),
                });
            }
            _ => {}
        }
    }
    None
}

/// Members whose name matches `query`, case-insensitively: a name or word
/// that starts with it first, then any that merely contain it.
pub(super) fn matches<'a>(roster: &'a [DraftMention], query: &str) -> Vec<&'a DraftMention> {
    let needle = query.to_lowercase();
    let mut starting = Vec::new();
    let mut containing = Vec::new();
    for member in roster {
        let name = member.display.to_lowercase();
        if needle.is_empty()
            || name.starts_with(&needle)
            || name
                .split_whitespace()
                .any(|word| word.starts_with(&needle))
        {
            starting.push(member);
        } else if name.contains(&needle) {
            containing.push(member);
        }
    }
    starting.extend(containing);
    starting.truncate(MAX_SHOWN);
    starting
}

/// Replaces the typed `@query` (from `trigger.start` to `cursor`) with
/// `@Name ` and returns the new text and the cursor after it.
pub(super) fn insert_mention(
    draft: &str,
    trigger: &Trigger,
    cursor: usize,
    display: &str,
) -> (String, usize) {
    let chars: Vec<char> = draft.chars().collect();
    let cursor = cursor.min(chars.len());
    let start = trigger.start.min(cursor);
    let mut text: String = chars[..start].iter().collect();
    let inserted = format!("@{display} ");
    text.push_str(&inserted);
    text.extend(&chars[cursor..]);
    (text, start + inserted.chars().count())
}

fn selected_id() -> egui::Id {
    egui::Id::new("remote_chat_mention_picker_selected")
}

fn dismissed_id() -> egui::Id {
    egui::Id::new("remote_chat_mention_picker_dismissed")
}

fn cursor_index(ctx: &egui::Context) -> Option<usize> {
    egui::TextEdit::load_state(ctx, remote_composer_id())?
        .cursor
        .char_range()
        .map(|range| range.primary.index)
}

/// What the picker shows this frame.
pub(super) struct PickerFrame {
    trigger: Trigger,
    candidates: Vec<DraftMention>,
    selected: usize,
}

/// Accepts one candidate: rewrites the draft, moves the cursor after the
/// inserted name, and remembers the member. A member that cannot be added
/// (the draft already holds the most Graph allows) leaves the draft as typed.
fn accept(
    ctx: &egui::Context,
    conversation: &mut RemoteConversation,
    trigger: &Trigger,
    member: &DraftMention,
) {
    let Some(cursor) = cursor_index(ctx) else {
        return;
    };
    if !conversation.add_draft_mention(member.clone()) {
        return;
    }
    let (text, new_cursor) =
        insert_mention(conversation.draft_mut(), trigger, cursor, &member.display);
    *conversation.draft_mut() = text;
    let id = remote_composer_id();
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
    state.store(ctx, id);
    ctx.memory_mut(|memory| memory.request_focus(id));
    ctx.data_mut(|data| data.insert_temp(selected_id(), 0_usize));
}

/// Runs before the text field is drawn, so a key the picker uses (Enter, Tab,
/// the arrows, Escape) never reaches the field and cannot send the message.
/// Returns what to draw after the field.
pub(super) fn begin(
    ctx: &egui::Context,
    conversation: &mut RemoteConversation,
) -> Option<PickerFrame> {
    if !conversation.can_mention() || !ctx.memory(|memory| memory.has_focus(remote_composer_id())) {
        return None;
    }
    let cursor = cursor_index(ctx)?;
    let Some(active) = trigger(conversation.draft_mut(), cursor) else {
        ctx.data_mut(|data| data.remove::<usize>(dismissed_id()));
        return None;
    };
    if ctx.data(|data| data.get_temp::<usize>(dismissed_id())) == Some(active.start) {
        return None;
    }
    let candidates: Vec<DraftMention> = matches(conversation.mention_roster(), &active.query)
        .into_iter()
        .cloned()
        .collect();
    if candidates.is_empty() {
        // Nothing to pick: either no roster yet (the hint shows) or no match.
        return Some(PickerFrame {
            trigger: active,
            candidates,
            selected: 0,
        });
    }
    let mut selected = ctx
        .data(|data| data.get_temp::<usize>(selected_id()))
        .unwrap_or(0)
        .min(candidates.len() - 1);
    let (down, up, accept_key, escape) = ctx.input_mut(|input| {
        (
            input.consume_key(Modifiers::NONE, Key::ArrowDown),
            input.consume_key(Modifiers::NONE, Key::ArrowUp),
            input.consume_key(Modifiers::NONE, Key::Enter)
                | input.consume_key(Modifiers::NONE, Key::Tab),
            input.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    if down {
        selected = (selected + 1) % candidates.len();
    }
    if up {
        selected = (selected + candidates.len() - 1) % candidates.len();
    }
    ctx.data_mut(|data| data.insert_temp(selected_id(), selected));
    if escape {
        ctx.data_mut(|data| data.insert_temp(dismissed_id(), active.start));
        return None;
    }
    if accept_key {
        let member = candidates[selected].clone();
        accept(ctx, conversation, &active, &member);
        return None;
    }
    Some(PickerFrame {
        trigger: active,
        candidates,
        selected,
    })
}

/// Draws the list (or the hint) above the text field. Returns true when the
/// user asked to open People.
pub(super) fn show(
    ui: &egui::Ui,
    conversation: &mut RemoteConversation,
    input: &egui::Response,
    frame: &PickerFrame,
    people_open: bool,
) -> bool {
    let ctx = ui.ctx().clone();
    let mut open_people = false;
    let mut chosen = None;
    let hint = frame.candidates.is_empty() && conversation.mention_roster().is_empty();
    if frame.candidates.is_empty() && !hint {
        return false;
    }
    egui::Area::new(egui::Id::new("remote_chat_mention_picker"))
        .order(Order::Foreground)
        .fixed_pos(input.rect.left_top() - egui::vec2(0.0, theme::space::XS))
        .pivot(Align2::LEFT_BOTTOM)
        .show(&ctx, |ui| {
            Frame::new()
                .fill(theme::surface())
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                .corner_radius(CornerRadius::same(theme::radius::MD))
                .inner_margin(Margin::symmetric(8, 6))
                .shadow(egui::Shadow {
                    offset: [0, 1],
                    blur: 4,
                    spread: 0,
                    color: theme::shadow(),
                })
                .show(ui, |ui| {
                    if hint {
                        ui.label(
                            RichText::new(if people_open {
                                "Loading people…"
                            } else {
                                "Open People to enable @mentions"
                            })
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                        );
                        if !people_open && ui.small_button("Open People").clicked() {
                            open_people = true;
                        }
                        return;
                    }
                    for (index, member) in frame.candidates.iter().enumerate() {
                        let selected = index == frame.selected;
                        let response = ui.add(
                            egui::Button::new(
                                RichText::new(&member.display)
                                    .font(theme::regular(theme::font_size::BODY))
                                    .color(theme::fg1()),
                            )
                            .fill(if selected {
                                theme::brand_tint()
                            } else {
                                theme::surface()
                            })
                            .frame(selected)
                            .min_size(egui::vec2(160.0, 0.0)),
                        );
                        // Chosen on press, not release: the press already
                        // moves keyboard focus off the text field.
                        if response.hovered() && ui.input(|input| input.pointer.primary_pressed()) {
                            chosen = Some(member.clone());
                        }
                    }
                });
        });
    if let Some(member) = chosen {
        accept(&ctx, conversation, &frame.trigger, &member);
    }
    open_people
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::MentionHandle;

    fn member(name: &str, raw: u64) -> DraftMention {
        DraftMention {
            handle: MentionHandle::for_test(raw),
            display: name.into(),
        }
    }

    #[test]
    fn a_trigger_needs_an_at_at_a_word_start_on_the_same_line() {
        assert_eq!(
            trigger("hello @bo", 9),
            Some(Trigger {
                start: 6,
                query: "bo".into()
            })
        );
        assert_eq!(
            trigger("@", 1),
            Some(Trigger {
                start: 0,
                query: String::new()
            })
        );
        assert_eq!(
            trigger("(@Ada Lo", 8),
            Some(Trigger {
                start: 1,
                query: "Ada Lo".into()
            })
        );
        // An email address, a different line and a far-back `@` are not triggers.
        assert_eq!(trigger("mail me@example", 15), None);
        assert_eq!(trigger("@bo\nnext", 8), None);
        assert_eq!(trigger("no at here", 10), None);
        let far = format!("@{}", "x".repeat(MAX_QUERY_CHARS + 5));
        assert_eq!(trigger(&far, far.chars().count()), None);
        // The cursor, not the end of the text, decides the query.
        assert_eq!(trigger("@ab cd", 3).map(|t| t.query), Some("ab".into()));
        assert_eq!(trigger("é @日", 4).map(|t| t.query), Some("日".into()));
    }

    #[test]
    fn matching_prefers_name_and_word_starts_and_is_bounded() {
        let roster = vec![
            member("Grace Hopper", 1),
            member("Ada Lovelace", 2),
            member("Adam Smith", 3),
            member("Tove Adams", 4),
            member("Bob Nadal", 5),
        ];
        let names = |query: &str| {
            matches(&roster, query)
                .into_iter()
                .map(|m| m.display.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names("ada"),
            ["Ada Lovelace", "Adam Smith", "Tove Adams", "Bob Nadal"]
        );
        assert_eq!(names("HOP"), ["Grace Hopper"]);
        assert_eq!(names("lace"), ["Ada Lovelace"]);
        assert_eq!(names("zzz"), Vec::<&str>::new());
        assert_eq!(names("").len(), 5);
        let many: Vec<_> = (0..20).map(|i| member(&format!("P{i}"), i)).collect();
        assert_eq!(matches(&many, "").len(), MAX_SHOWN);
    }

    #[test]
    fn inserting_replaces_the_typed_query_and_places_the_cursor() {
        let draft = "hi @ad there";
        let active = trigger(draft, 6).unwrap();
        let (text, cursor) = insert_mention(draft, &active, 6, "Ada Lovelace");
        assert_eq!(text, "hi @Ada Lovelace  there");
        assert_eq!(cursor, "hi @Ada Lovelace ".chars().count());
        let (text, cursor) = insert_mention("@", &trigger("@", 1).unwrap(), 1, "日本");
        assert_eq!(text, "@日本 ");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn a_picked_mention_composes_into_a_mention_at_send() {
        use crate::integrations::ComposedBody;
        let picked = vec![member("Ada Lovelace", 7)];
        let draft = "hi @Ada Lovelace  there";
        let body = ComposedBody::compose(draft, &picked);
        assert_eq!(body.mentions(), vec![picked[0].handle]);
        assert_eq!(body.plain_projection(), "hi Ada Lovelace  there");
    }
}
