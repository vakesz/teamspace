use crate::shared::model::{Message, MessageAttachment};
use crate::ui::a11y::clickable;
use crate::ui::message_nav::{Entry, MessageNav, NavOutcome};
use crate::ui::message_view::{MessageAction, MessageOptions, draw_message_tracked};
use crate::ui::theme;
use eframe::egui::{self, Layout, RichText, Stroke};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SharedTabAction {
    PreviewAttachment(MessageAttachment),
    SelectLink(String),
}

pub(crate) fn draw_shared_tab(
    ui: &mut egui::Ui,
    selected_name: &str,
    messages: &[Message],
) -> Option<SharedTabAction> {
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Shared in this chat")
                    .font(theme::regular(theme::font_size::TITLE))
                    .strong()
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(format!("Files and links shared with {selected_name}"))
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    });
    ui.add_space(20.0);
    let shared_items = shared_items(messages);
    if shared_items.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(45.0);
            ui.label(
                RichText::new("Nothing has been shared in this chat yet.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
        return None;
    }
    let mut action = None;
    for item in shared_items {
        let response = egui::Frame::new()
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .corner_radius(9.0)
            .inner_margin(egui::Margin::symmetric(18, 13))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(item.icon)
                            .font(theme::regular(theme::font_size::TITLE))
                            .color(theme::brand_fg()),
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(item.title)
                                .font(theme::semibold(theme::font_size::BODY))
                                .color(theme::fg1()),
                        );
                        ui.label(
                            RichText::new(item.detail)
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(item.kind)
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                });
            });
        if clickable(
            ui,
            response.response,
            &format!("{}: {}", item.kind, item.title),
        )
        .on_hover_text(match &item.action {
            SharedTabAction::PreviewAttachment(_) => "Open local preview",
            SharedTabAction::SelectLink(_) => "Open local link details",
        })
        .clicked()
        {
            action = Some(item.action);
        }
        ui.add_space(8.0);
    }
    action
}

pub(crate) fn draw_notes_tab(ui: &mut egui::Ui, notes: &mut String) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Conversation notes")
                    .font(theme::regular(theme::font_size::TITLE))
                    .strong()
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("A private scratchpad for this chat")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    });
    ui.add_space(18.0);
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(9.0)
        .inner_margin(egui::Margin::symmetric(18, 14))
        .show(ui, |ui| {
            changed |= ui
                .add(
                    egui::TextEdit::multiline(notes)
                        .desired_rows(10)
                        .hint_text("Add a note, reminder, or follow-up for this conversation…")
                        .frame(false),
                )
                .changed();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Notes are saved in this local profile.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    changed
}

pub(crate) fn draw_recap_tab(ui: &mut egui::Ui, messages: &[Message]) {
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Chat recap")
                    .font(theme::semibold(theme::font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("A quick overview of recent messages")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    });
    ui.add_space(18.0);
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(9.0)
        .inner_margin(egui::Margin::symmetric(20, 18))
        .show(ui, |ui| {
            let recap = recap_for(messages);
            ui.label(
                RichText::new("Conversation details")
                    .font(theme::regular(theme::font_size::BODY))
                    .strong()
                    .color(theme::fg1()),
            );
            ui.add_space(10.0);
            for (label, summary) in [
                ("Messages", recap.message_count.to_string()),
                ("Participants", recap.participants),
                ("Shared items", recap.shared_item_count.to_string()),
                ("Latest activity", recap.latest_activity),
            ] {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("•")
                            .font(theme::regular(theme::font_size::SUBTITLE))
                            .color(theme::brand_fg()),
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(label)
                                .font(theme::semibold(theme::font_size::CAPTION))
                                .color(theme::fg1()),
                        );
                        ui.label(
                            RichText::new(summary)
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                });
                ui.add_space(12.0);
            }
            ui.label(
                RichText::new("Summary based only on messages kept in this local session.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
}

pub(crate) fn draw_storyline_tab(
    ui: &mut egui::Ui,
    messages: &[Message],
    composer: egui::Id,
) -> Option<(usize, MessageAction)> {
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Conversation timeline")
                    .font(theme::regular(theme::font_size::TITLE))
                    .strong()
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("Messages in chronological order")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    });
    ui.add_space(18.0);
    let mut selected_action = None;
    let mut nav = MessageNav::default();
    for (index, message) in messages.iter().enumerate() {
        let (action, item) = draw_message_tracked(
            ui,
            ("shared_tab_message", index),
            message,
            MessageOptions::default(),
        );
        nav.push(item);
        if let Some(action) = action {
            selected_action = Some((index, action));
        }
    }
    let entry = if ui.memory(|memory| memory.focused().is_none())
        && ui.rect_contains_pointer(ui.clip_rect())
    {
        Entry::Anywhere
    } else {
        Entry::Never
    };
    let _: NavOutcome = nav.handle(ui, composer, entry);
    selected_action
}

pub(crate) fn draw_info_tab(ui: &mut egui::Ui, selected_name: &str, messages: &[Message]) {
    let recap = recap_for(messages);
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new("Conversation info")
                    .font(theme::regular(theme::font_size::TITLE))
                    .strong()
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(selected_name)
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
    });
    ui.add_space(18.0);
    egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(9.0)
        .inner_margin(egui::Margin::symmetric(18, 15))
        .show(ui, |ui| {
            for (label, value) in [
                ("People", recap.participants),
                ("Messages in this session", recap.message_count.to_string()),
                ("Shared items", recap.shared_item_count.to_string()),
            ] {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(label)
                            .font(theme::semibold(theme::font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(value)
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                });
                ui.add_space(8.0);
            }
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Conversation information is kept locally in this preview.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
}

pub(crate) struct SharedItem<'a> {
    pub(crate) icon: &'static str,
    pub(crate) title: &'a str,
    pub(crate) detail: String,
    pub(crate) kind: &'static str,
    pub(crate) action: SharedTabAction,
}

pub(crate) fn shared_items(messages: &[Message]) -> Vec<SharedItem<'_>> {
    let mut items = Vec::new();
    for message in messages {
        if let Some(attachment) = message.attachment {
            items.push(SharedItem {
                icon: "▧",
                title: attachment.label(),
                detail: format!("Shared by {} · {}", message.name, message.time),
                kind: "PNG image",
                action: SharedTabAction::PreviewAttachment(attachment),
            });
        }
        if message.text.contains("://") {
            items.push(SharedItem {
                icon: "↗",
                title: &message.text,
                detail: format!("Shared by {} · {}", message.name, message.time),
                kind: "Link",
                action: SharedTabAction::SelectLink(message.text.clone()),
            });
        }
    }
    items
}

pub(crate) struct ConversationRecap {
    pub(crate) message_count: usize,
    pub(crate) participants: String,
    pub(crate) shared_item_count: usize,
    pub(crate) latest_activity: String,
}

pub(crate) fn recap_for(messages: &[Message]) -> ConversationRecap {
    let mut participants = Vec::new();
    for message in messages {
        if !participants.iter().any(|name: &&str| *name == message.name) {
            participants.push(message.name.as_str());
        }
    }
    let latest_activity = messages.last().map_or_else(
        || "No messages yet".into(),
        |message| format!("{} · {}", message.name, message.time),
    );
    ConversationRecap {
        message_count: messages.len(),
        participants: if participants.is_empty() {
            "No participants yet".into()
        } else {
            participants.join(", ")
        },
        shared_item_count: shared_items(messages).len(),
        latest_activity,
    }
}

#[cfg(test)]
mod tests {
    use super::{SharedTabAction, shared_items};
    use crate::shared::model::{Message, MessageAttachment};
    use eframe::egui::Color32;

    #[test]
    fn shared_items_keep_a_typed_action_for_attachments_and_links() {
        let messages = vec![Message {
            name: "You".into(),
            time: "Now".into(),
            text: "https://example.invalid/reference".into(),
            color: Color32::GRAY,
            own: true,
            attachment: Some(MessageAttachment::LayoutMockup),
            day_divider_before: false,
            reply_context: None,
            reply_context_extra: 0,
            file_chips: Vec::new(),
            rich: None,
            reactions: Vec::new(),
        }];

        let items = shared_items(&messages);
        assert!(matches!(
            items[0].action,
            SharedTabAction::PreviewAttachment(MessageAttachment::LayoutMockup)
        ));
        assert!(matches!(
            &items[1].action,
            SharedTabAction::SelectLink(link) if link == "https://example.invalid/reference"
        ));
    }

    fn plain(text: &str) -> Message {
        Message {
            name: "Tove Ashworth".into(),
            time: "Now".into(),
            text: text.into(),
            color: Color32::GRAY,
            own: false,
            attachment: None,
            day_divider_before: false,
            reply_context: None,
            reply_context_extra: 0,
            file_chips: Vec::new(),
            rich: None,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn storyline_messages_are_navigable_and_escape_returns_to_the_composer() {
        let composer = eframe::egui::Id::new("storyline_test_composer");
        let messages = vec![plain("one"), plain("two"), plain("three")];
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, draft: &mut String| {
                let key = eframe::egui::Id::new("storyline_theme");
                if ui.ctx().data(|data| data.get_temp::<bool>(key)).is_none() {
                    crate::ui::theme::apply(ui.ctx());
                    ui.ctx().data_mut(|data| data.insert_temp(key, true));
                    ui.ctx().request_discard("install theme fonts");
                    return;
                }
                let _ = super::draw_storyline_tab(ui, &messages, composer);
                ui.add(eframe::egui::TextEdit::singleline(draft).id(composer));
            },
            String::new(),
        );
        harness.run();
        harness.hover_at(eframe::egui::pos2(100.0, 100.0));
        harness.run();
        let focused = |h: &egui_kittest::Harness<'_, String>| h.ctx.memory(|m| m.focused());

        harness.key_press(eframe::egui::Key::K);
        harness.run();
        let newest = focused(&harness);
        assert!(newest.is_some() && newest != Some(composer));

        harness.key_press(eframe::egui::Key::K);
        harness.run();
        assert_ne!(focused(&harness), newest);

        harness.key_press(eframe::egui::Key::Escape);
        harness.run();
        assert_eq!(focused(&harness), Some(composer));
    }
}
