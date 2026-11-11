use super::avatar::{Avatar, AvatarSize};
use super::icon::{Icon, IconSize};
use super::message_nav::{NavItem, focus_target};
use super::theme;
use crate::data::shared_files::shared_file_for_message_attachment;
use crate::shared::model::{Message, MessageAttachment, MessageReaction};
use crate::shared::reaction::ReactionKind;
use eframe::egui::{
    self, Align, CornerRadius, Id, Layout, Rect, RichText, Sense, Shadow, Stroke, StrokeKind,
    WidgetInfo, WidgetType,
};
use std::hash::Hash;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageAction {
    PreviewAttachment(MessageAttachment),
    SelectLink,
    ToggleReaction(ReactionKind),
    /// Only produced for messages drawn with `MessageOptions::editable`.
    Edit,
    /// Only produced for messages drawn with `MessageOptions::editable`.
    Delete,
    /// A link run of a formatted message was clicked. Carries only the run's
    /// index; the owner resolves the destination from its own message row.
    OpenRichLink {
        run: usize,
    },
}

/// How a timeline entry is presented relative to its neighbours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageOptions {
    /// Draw the avatar and the name/time caption. Consecutive messages from
    /// one sender omit it; the first message after a day divider or a
    /// different sender must keep it.
    pub(crate) show_header: bool,
    /// Offer Edit and Delete in the hover toolbar.
    pub(crate) editable: bool,
    /// Render the local reaction pills under the bubble. A caller with a
    /// target-bound provider reaction affordance disables this and draws its
    /// own pills.
    pub(crate) reaction_pills_enabled: bool,
    /// Draw the whole hover toolbar: the generic reaction picker and, when
    /// `editable`, the Edit/Delete menu. A caller with a target-bound
    /// provider affordance must disable this so the shared widget cannot
    /// offer actions it cannot reconcile.
    pub(crate) hover_toolbar_enabled: bool,
}

impl Default for MessageOptions {
    fn default() -> Self {
        Self {
            show_header: true,
            editable: false,
            reaction_pills_enabled: true,
            hover_toolbar_enabled: true,
        }
    }
}

const AVATAR_COLUMN: f32 = 32.0;
const TOOL_SIZE: f32 = 24.0;
const BUBBLE_MAX_FRACTION: f32 = 0.6;
const REPLY_BAR_WIDTH: f32 = 3.0;

/// Renders one chat timeline entry and reports the selected local action.
/// `id_salt` must identify the message stably across frames (not its screen
/// position), because the hover toolbar's widget ids derive from it.
#[cfg(test)]
pub(crate) fn draw_message(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    msg: &Message,
) -> Option<MessageAction> {
    draw_message_with(ui, id_salt, msg, MessageOptions::default())
}

#[cfg(test)]
pub(crate) fn draw_message_with(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    msg: &Message,
    options: MessageOptions,
) -> Option<MessageAction> {
    draw_message_tracked(ui, id_salt, msg, options).0
}

/// Like [`draw_message_with`], and also reports the message's keyboard focus
/// target so a list can navigate between messages with
/// [`MessageNav`](crate::ui::message_nav::MessageNav).
pub(crate) fn draw_message_tracked(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    msg: &Message,
    options: MessageOptions,
) -> (Option<MessageAction>, NavItem) {
    ui.push_id(id_salt, |ui| {
        let base = ui.id();
        let tools = base.with("message_tools");
        let action = draw_message_scoped(ui, msg, options, tools);
        let rect = ui.min_rect();
        let id = base.with("message_focus");
        focus_target(
            ui,
            rect,
            id,
            &format!("Message from {}, {}", msg.name, msg.time),
        );
        let react = tools.with("react");
        let item = NavItem {
            id,
            rect,
            activate: options.hover_toolbar_enabled.then_some(react),
            secondary: (options.hover_toolbar_enabled && options.editable)
                .then(|| tools.with("more")),
        };
        (action, item)
    })
    .inner
}

fn draw_message_scoped(
    ui: &mut egui::Ui,
    msg: &Message,
    options: MessageOptions,
    tools: Id,
) -> Option<MessageAction> {
    let mut selected_action = None;
    if options.show_header {
        ui.add_space(theme::space::SM);
    }
    let available = ui.available_width();
    let max_width = (available * BUBBLE_MAX_FRACTION)
        .clamp(200.0, 640.0)
        .min((available - AVATAR_COLUMN - 2.0 * TOOL_SIZE - 16.0).max(120.0));
    if msg.own {
        ui.with_layout(Layout::top_down(Align::Max), |ui| {
            draw_column(ui, msg, options, (max_width, tools), &mut selected_action);
        });
    } else {
        ui.horizontal_top(|ui| {
            if options.show_header {
                Avatar::new(&msg.name, AvatarSize::Md32).show(ui);
            } else {
                ui.add_space(AVATAR_COLUMN);
            }
            ui.vertical(|ui| {
                draw_column(ui, msg, options, (max_width, tools), &mut selected_action);
            });
        });
    }
    selected_action
}

fn draw_column(
    ui: &mut egui::Ui,
    msg: &Message,
    options: MessageOptions,
    (max_width, tools): (f32, Id),
    selected_action: &mut Option<MessageAction>,
) {
    let caption = theme::regular(theme::font_size::CAPTION);
    if options.show_header {
        if msg.own {
            ui.label(RichText::new(&msg.time).font(caption).color(theme::fg3()));
        } else {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&msg.name)
                        .font(theme::semibold(theme::font_size::CAPTION))
                        .color(theme::fg1()),
                );
                ui.label(RichText::new(&msg.time).font(caption).color(theme::fg3()));
            });
        }
    }
    if let Some(attachment) = msg.attachment {
        draw_attachment(ui, attachment, selected_action);
        ui.add_space(3.0);
    }
    let bubble = draw_bubble(ui, msg, max_width, selected_action);
    draw_file_chips(ui, msg);
    if options.reaction_pills_enabled && !msg.reactions.is_empty() {
        ui.add_space(3.0);
        if msg.own {
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                for reaction in msg.reactions.iter().rev() {
                    draw_reaction_pill(ui, reaction, selected_action);
                }
            });
        } else {
            ui.horizontal(|ui| {
                for reaction in &msg.reactions {
                    draw_reaction_pill(ui, reaction, selected_action);
                }
            });
        }
    }
    if options.hover_toolbar_enabled {
        draw_hover_toolbar(ui, msg, options, bubble, tools, selected_action);
    }
}

/// Read-only name chips for the files a received message links. They are
/// labels, not links: opening or downloading a provider file is not offered.
fn draw_file_chips(ui: &mut egui::Ui, msg: &Message) {
    for name in &msg.file_chips {
        ui.add_space(3.0);
        let chip = egui::Frame::new()
            .fill(theme::surface_hover())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .corner_radius(egui::CornerRadius::same(theme::radius::SM))
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(Icon::Attach.image(IconSize::S16));
                    ui.label(
                        RichText::new(name)
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg1()),
                    );
                });
            });
        chip.response.widget_info(|| {
            WidgetInfo::labeled(WidgetType::Label, true, format!("Attachment {name}"))
        });
    }
}

fn draw_attachment(
    ui: &mut egui::Ui,
    attachment: MessageAttachment,
    selected_action: &mut Option<MessageAction>,
) {
    let size = match attachment {
        MessageAttachment::LayoutMockup => egui::vec2(286.0, 168.0),
        MessageAttachment::Spreadsheet => egui::vec2(330.0, 195.0),
    };
    let Some(file) = shared_file_for_message_attachment(attachment) else {
        return;
    };
    let rounding = CornerRadius::same(theme::radius::LG);
    let response = ui.add(
        egui::Image::from_bytes(file.asset.byte_uri, file.asset.bytes)
            .fit_to_exact_size(size)
            .corner_radius(rounding),
    );
    ui.painter().rect_stroke(
        response.rect,
        rounding,
        Stroke::new(1.0_f32, theme::stroke_subtle()),
        StrokeKind::Inside,
    );
    let label = format!("Open local preview: {}", file.name);
    let response = response.interact(Sense::click());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), &label));
    if response.on_hover_text(&label).clicked() {
        *selected_action = Some(MessageAction::PreviewAttachment(attachment));
    }
}

/// Draws the text bubble (with its optional reply quote) and returns its rect.
fn draw_bubble(
    ui: &mut egui::Ui,
    msg: &Message,
    max_width: f32,
    selected_action: &mut Option<MessageAction>,
) -> Rect {
    let frame = egui::Frame::new()
        .corner_radius(CornerRadius::same(theme::radius::LG))
        .inner_margin(egui::Margin::symmetric(12, 8));
    let frame = if msg.own {
        frame.fill(theme::outgoing_bubble())
    } else {
        frame
            .fill(theme::incoming_bubble())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .shadow(Shadow {
                offset: [0, 1],
                blur: 2,
                spread: 0,
                color: theme::shadow(),
            })
    };
    frame
        .show(ui, |ui| {
            ui.set_max_width(max_width);
            if let Some(reply) = &msg.reply_context {
                let quote = egui::Frame::new()
                    .fill(theme::surface_subtle())
                    .corner_radius(CornerRadius::same(theme::radius::SM))
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 8,
                        top: 6,
                        bottom: 6,
                    })
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(if reply.time.is_empty() {
                                reply.name.clone()
                            } else {
                                format!("{}  {}", reply.name, reply.time)
                            })
                            .font(theme::semibold(theme::font_size::CAPTION))
                            .color(theme::fg2()),
                        );
                        ui.label(
                            RichText::new(&reply.text)
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg1()),
                        );
                    });
                if msg.reply_context_extra > 0 {
                    ui.label(
                        RichText::new(format!("+{} more quoted", msg.reply_context_extra))
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg2()),
                    );
                }
                let rect = quote.response.rect;
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        rect.left_top(),
                        egui::pos2(rect.left() + REPLY_BAR_WIDTH, rect.bottom()),
                    ),
                    CornerRadius::same(2),
                    theme::stroke(),
                );
                ui.add_space(4.0);
            }
            let body = theme::regular(theme::font_size::BODY);
            if let Some(runs) = &msg.rich {
                if let Some(run) = super::rich_text::draw_rich(ui, runs) {
                    *selected_action = Some(MessageAction::OpenRichLink { run });
                }
            } else if msg.text.contains("://") {
                if ui
                    .link(RichText::new(&msg.text).font(body).color(theme::brand_fg()))
                    .on_hover_text("Open local link details")
                    .clicked()
                {
                    *selected_action = Some(MessageAction::SelectLink);
                }
            } else {
                ui.label(RichText::new(&msg.text).font(body).color(theme::fg1()));
            }
        })
        .response
        .rect
}

fn draw_reaction_pill(
    ui: &mut egui::Ui,
    reaction: &MessageReaction,
    selected_action: &mut Option<MessageAction>,
) {
    let selected = reaction.reacted_by_current_user;
    let response = ui.add(
        egui::Button::new(
            RichText::new(format!("{} {}", reaction.kind.icon(), reaction.count))
                .font(theme::regular(theme::font_size::CAPTION))
                .color(theme::fg1()),
        )
        .fill(if selected {
            theme::brand_tint()
        } else {
            theme::surface()
        })
        .stroke(Stroke::new(
            1.0_f32,
            if selected {
                theme::brand_fg()
            } else {
                theme::stroke_subtle()
            },
        ))
        .corner_radius(CornerRadius::same(10)),
    );
    if response
        .on_hover_text(format!(
            "{} {} reaction",
            if selected { "Remove your" } else { "Add a" },
            reaction.kind.label()
        ))
        .clicked()
    {
        *selected_action = Some(MessageAction::ToggleReaction(reaction.kind));
    }
}

/// Hover toolbar beside the bubble: add a reaction and, for local messages,
/// edit or delete. The buttons never take layout space; while hidden they stay
/// in the focus order so keyboard users can reach them, and focus reveals them.
fn draw_hover_toolbar(
    ui: &mut egui::Ui,
    msg: &Message,
    options: MessageOptions,
    bubble: Rect,
    base: Id,
    selected_action: &mut Option<MessageAction>,
) {
    let tool_count = if options.editable { 2.0 } else { 1.0 };
    let toolbar_width = tool_count * TOOL_SIZE;
    let left = if msg.own {
        bubble.left() - 4.0 - toolbar_width
    } else {
        bubble.right() + 4.0
    };
    let top = bubble.top();
    let react_rect = Rect::from_min_size(egui::pos2(left, top), egui::vec2(TOOL_SIZE, TOOL_SIZE));
    let more_rect = react_rect.translate(egui::vec2(TOOL_SIZE, 0.0));
    let zone = bubble.union(Rect::from_min_size(
        egui::pos2(left, top),
        egui::vec2(toolbar_width, TOOL_SIZE),
    ));

    let react_id = base.with("react");
    let more_id = base.with("more");
    let popup_open = |id: Id| egui::Popup::is_id_open(ui.ctx(), id.with("popup"));
    let focused = ui.memory(|memory| memory.has_focus(react_id) || memory.has_focus(more_id));
    let visible = ui.rect_contains_pointer(zone)
        || focused
        || popup_open(react_id)
        || (options.editable && popup_open(more_id));

    let react = hover_tool(
        ui,
        react_rect,
        react_id,
        Icon::Emoji,
        "Add reaction",
        visible,
    );
    egui::Popup::menu(&react).show(|ui| {
        for kind in ReactionKind::ALL {
            if ui
                .button(format!("{}  {}", kind.icon(), kind.label()))
                .clicked()
            {
                *selected_action = Some(MessageAction::ToggleReaction(kind));
                ui.close();
            }
        }
    });
    if options.editable {
        let more = hover_tool(
            ui,
            more_rect,
            more_id,
            Icon::More,
            "Message options",
            visible,
        );
        egui::Popup::menu(&more).show(|ui| {
            if ui.button("Edit message").clicked() {
                *selected_action = Some(MessageAction::Edit);
                ui.close();
            }
            if ui.button("Delete message").clicked() {
                *selected_action = Some(MessageAction::Delete);
                ui.close();
            }
        });
    }
}

fn hover_tool(
    ui: &mut egui::Ui,
    rect: Rect,
    id: Id,
    icon: Icon,
    label: &str,
    visible: bool,
) -> egui::Response {
    let sense = if visible {
        Sense::click()
    } else {
        Sense::focusable_noninteractive()
    };
    let response = ui.interact(rect, id, sense);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), label));
    if visible && ui.is_rect_visible(rect) {
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::radius::SM),
            if response.hovered() {
                theme::surface_hover()
            } else {
                theme::surface()
            },
            Stroke::new(1.0_f32, theme::stroke_subtle()),
            StrokeKind::Inside,
        );
        let side = IconSize::S16.px();
        icon.image(IconSize::S16).paint_at(
            ui,
            Rect::from_center_size(rect.center(), egui::vec2(side, side)),
        );
    }
    response.on_hover_text(label)
}

#[cfg(test)]
mod tests {
    use super::{
        MessageAction, MessageOptions, draw_message, draw_message_tracked, draw_message_with,
    };
    use crate::shared::model::{Message, MessageAttachment, MessageReaction};
    use crate::shared::reaction::ReactionKind;
    use crate::ui::message_nav::{Entry, MessageNav};
    use eframe::egui::{self, Color32};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    /// Installs the Inter families on the harness context. Fonts apply on the
    /// next pass, so the first pass is discarded before anything is drawn.
    fn ensure_theme(ui: &egui::Ui) -> bool {
        let key = egui::Id::new("message_view_test_theme");
        if ui.ctx().data(|data| data.get_temp::<bool>(key)).is_some() {
            return true;
        }
        crate::ui::theme::apply(ui.ctx());
        ui.ctx().data_mut(|data| data.insert_temp(key, true));
        ui.ctx().request_discard("install theme fonts");
        false
    }

    fn message(text: &str) -> Message {
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

    fn harness(
        message: Message,
        options: MessageOptions,
    ) -> Harness<'static, Option<MessageAction>> {
        Harness::new_ui_state(
            move |ui, state: &mut Option<MessageAction>| {
                if !ensure_theme(ui) {
                    return;
                }
                if let Some(action) = draw_message_with(ui, "message", &message, options) {
                    *state = Some(action);
                }
            },
            None,
        )
    }

    #[test]
    fn clicking_an_inline_attachment_requests_its_local_preview() {
        let mut message = message("Here is the current layout draft.");
        message.attachment = Some(MessageAttachment::LayoutMockup);
        let mut harness = harness(message, MessageOptions::default());
        harness.get_by_label_contains("Open local preview").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            Some(MessageAction::PreviewAttachment(
                MessageAttachment::LayoutMockup
            ))
        );
    }

    #[test]
    fn clicking_a_reaction_pill_requests_its_toggle_action() {
        let mut message = message("Acknowledged.");
        message.reactions = vec![MessageReaction {
            kind: ReactionKind::Like,
            count: 1,
            reacted_by_current_user: false,
        }];
        let mut harness = harness(message, MessageOptions::default());
        harness.get_by_label_contains("1").click();
        harness.run();
        assert_eq!(
            *harness.state(),
            Some(MessageAction::ToggleReaction(ReactionKind::Like))
        );
    }

    #[test]
    fn clicking_an_inline_link_requests_the_local_link_action() {
        let mut harness = harness(
            message("https://example.invalid/reference"),
            MessageOptions::default(),
        );
        harness
            .get_by_label("https://example.invalid/reference")
            .click();
        harness.run();
        assert_eq!(*harness.state(), Some(MessageAction::SelectLink));
    }

    #[test]
    fn add_reaction_stays_reachable_without_hover() {
        let mut harness = harness(message("Hello"), MessageOptions::default());
        harness.run();
        // Hidden until hover or focus, but still exposed to assistive tech.
        harness.get_by_label("Add reaction");
    }

    fn reacted_message() -> Message {
        let mut message = message("Provider-projected reaction.");
        message.reactions = vec![MessageReaction {
            kind: ReactionKind::Like,
            count: 1,
            reacted_by_current_user: true,
        }];
        message
    }

    fn options(pills: bool, toolbar: bool) -> MessageOptions {
        MessageOptions {
            show_header: true,
            editable: false,
            reaction_pills_enabled: pills,
            hover_toolbar_enabled: toolbar,
        }
    }

    #[test]
    fn provider_scoped_messages_disable_both_shared_pills_and_hover_toolbar() {
        let mut harness = harness(reacted_message(), options(false, false));
        harness.run();
        assert!(harness.query_by_label("Add reaction").is_none());
        assert!(harness.query_by_label_contains("1").is_none());
    }

    #[test]
    fn the_hover_toolbar_and_the_reaction_pills_are_independent_flags() {
        let mut toolbar_only = harness(reacted_message(), options(false, true));
        toolbar_only.run();
        toolbar_only.get_by_label("Add reaction");
        assert!(toolbar_only.query_by_label_contains("1").is_none());

        let mut pills_only = harness(reacted_message(), options(true, false));
        pills_only.run();
        assert!(pills_only.query_by_label("Add reaction").is_none());
        pills_only.get_by_label_contains("1");
    }

    #[test]
    fn grouped_messages_omit_the_sender_header() {
        let mut harness = Harness::new_ui(|ui| {
            if !ensure_theme(ui) {
                return;
            }
            let message = message("Same sender");
            draw_message_with(
                ui,
                "first",
                &message,
                MessageOptions {
                    show_header: false,
                    editable: false,
                    reaction_pills_enabled: true,
                    hover_toolbar_enabled: true,
                },
            );
            draw_message(ui, "second", &message);
        });
        harness.run();
        assert_eq!(harness.query_all_by_label("Tove Ashworth").count(), 1);
    }

    #[test]
    fn own_messages_show_a_timestamp_but_no_you_label() {
        let mut own = message("Mine");
        own.own = true;
        own.name = "You".into();
        let mut harness = harness(own, MessageOptions::default());
        harness.run();
        harness.get_by_label("Now");
        assert!(harness.query_by_label("You").is_none());
    }

    #[test]
    fn editable_messages_expose_message_options() {
        let mut own = message("Mine");
        own.own = true;
        let mut harness = harness(
            own,
            MessageOptions {
                show_header: true,
                editable: true,
                reaction_pills_enabled: true,
                hover_toolbar_enabled: true,
            },
        );
        harness.run();
        harness.get_by_label("Message options");
    }

    /// Three messages above a single-line composer, wired like the chat pane.
    struct NavState {
        ids: Vec<egui::Id>,
        draft: String,
        entry: Entry,
    }

    fn composer_id() -> egui::Id {
        egui::Id::new("nav_test_composer")
    }

    fn nav_harness(entry: Entry) -> Harness<'static, NavState> {
        Harness::new_ui_state(
            |ui, state: &mut NavState| {
                if !ensure_theme(ui) {
                    return;
                }
                state.ids.clear();
                let mut nav = MessageNav::default();
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for index in 0..3 {
                            let (_, item) = draw_message_tracked(
                                ui,
                                ("nav", index),
                                &message(&format!("Message number {index}")),
                                MessageOptions::default(),
                            );
                            state.ids.push(item.id);
                            nav.push(item);
                        }
                        nav.handle(ui, composer_id(), state.entry);
                    });
                // A focusable widget between the list and the composer, so a
                // test cannot pass by egui's own "nearest widget above" move.
                let _ = ui.button("Between");
                ui.add(egui::TextEdit::singleline(&mut state.draft).id(composer_id()));
            },
            NavState {
                ids: Vec::new(),
                draft: String::new(),
                entry,
            },
        )
    }

    fn focused_message(harness: &Harness<'static, NavState>) -> Option<usize> {
        harness
            .state()
            .ids
            .iter()
            .position(|id| harness.ctx.memory(|memory| memory.has_focus(*id)))
    }

    fn focus(harness: &mut Harness<'static, NavState>, id: egui::Id) {
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        harness.run();
    }

    #[test]
    fn arrows_and_jk_move_between_messages_and_clamp_at_the_ends() {
        let mut harness = nav_harness(Entry::Never);
        harness.run();
        let first = harness.state().ids[0];
        focus(&mut harness, first);
        assert_eq!(focused_message(&harness), Some(0));

        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(focused_message(&harness), Some(1));
        harness.key_press(egui::Key::J);
        harness.run();
        assert_eq!(focused_message(&harness), Some(2));
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(focused_message(&harness), Some(2));
        harness.key_press(egui::Key::K);
        harness.run();
        assert_eq!(focused_message(&harness), Some(1));
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(focused_message(&harness), Some(0));
    }

    #[test]
    fn enter_opens_the_toolbar_and_escape_walks_back_to_the_composer() {
        let mut harness = nav_harness(Entry::Never);
        harness.run();
        let ids = harness.state().ids.clone();
        focus(&mut harness, ids[1]);

        harness.key_press(egui::Key::Enter);
        harness.run();
        // Focus left the message row for its toolbar.
        assert_eq!(focused_message(&harness), None);
        assert!(harness.ctx.memory(|memory| memory.focused()).is_some());

        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(focused_message(&harness), Some(1));

        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(composer_id())
        );
    }

    #[test]
    fn typing_in_the_composer_never_moves_message_focus() {
        let mut harness = nav_harness(Entry::FromComposer);
        harness.run();
        focus(&mut harness, composer_id());
        for key in [egui::Key::J, egui::Key::K, egui::Key::ArrowDown] {
            harness.key_press(key);
            harness.run();
            assert_eq!(focused_message(&harness), None);
        }
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(composer_id())
        );
    }

    #[test]
    fn up_from_an_empty_composer_enters_at_the_newest_message() {
        let mut harness = nav_harness(Entry::FromComposer);
        harness.run();
        focus(&mut harness, composer_id());
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(focused_message(&harness), Some(2));
    }

    #[test]
    fn k_and_j_enter_the_list_only_when_nothing_holds_focus() {
        let mut harness = nav_harness(Entry::Anywhere);
        harness.run();
        harness.key_press(egui::Key::K);
        harness.run();
        assert_eq!(focused_message(&harness), Some(2));

        let mut harness = nav_harness(Entry::Anywhere);
        harness.run();
        harness.key_press(egui::Key::J);
        harness.run();
        assert_eq!(focused_message(&harness), Some(0));
    }

    #[test]
    fn unfocused_navigation_stays_inert_without_an_entry() {
        let mut harness = nav_harness(Entry::Never);
        harness.run();
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(focused_message(&harness), None);
    }
}
