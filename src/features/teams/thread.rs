//! Post cards and inline thread rendering for the Teams feature.

use super::TeamsState;
use crate::data::team_catalog::SourceReply;
use crate::data::workspace::workspace;
use crate::features::threads::{ThreadStore, ThreadTarget};
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, icon_button};
use crate::ui::list_row::layout_truncated;
use crate::ui::message_nav::{Entry, MessageNav, NavItem, NavOutcome, focus_target};
use crate::ui::reactions::{ReactionSet, draw_reaction_bar};
use crate::ui::shortcuts::{multiline_submit_pressed, take_escape};
use crate::ui::theme::{self, font_size, radius, space};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, CornerRadius, Layout, RichText, Stroke};

pub(super) struct TeamPost<'a> {
    /// Stable post key, identifying the card for keyboard navigation.
    pub(super) key: &'a str,
    pub(super) author: &'a str,
    pub(super) date: &'a str,
    pub(super) title: &'a str,
    pub(super) body: &'a str,
    pub(super) link: Option<&'a str>,
    pub(super) reactions: &'a ReactionSet,
    pub(super) source_reply_count: usize,
    pub(super) source_replies: &'a [SourceReply],
    pub(super) local_reply_count: usize,
    pub(super) locally_authored: bool,
}

#[derive(Default)]
pub(super) struct TeamPostAction {
    pub(super) open_thread: bool,
    pub(super) link: Option<String>,
    pub(super) reaction: Option<String>,
    pub(super) edit_local_post: bool,
    pub(super) delete_local_post: bool,
    /// The card's keyboard focus target, for list navigation.
    pub(super) nav_item: Option<NavItem>,
}

pub(super) fn thread_reply_link_label(reply_count: usize) -> String {
    match reply_count {
        1 => "Open 1 reply".into(),
        count => format!("Open {count} replies"),
    }
}

pub(super) fn draw_team_post(ui: &mut egui::Ui, post: TeamPost<'_>) -> TeamPostAction {
    let mut action = TeamPostAction::default();
    let card = egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(CornerRadius::same(radius::LG))
        .inner_margin(egui::Margin::symmetric(20, 16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                Avatar::new(post.author, AvatarSize::Md32).show(ui);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(
                        RichText::new(post.author)
                            .font(theme::semibold(font_size::BODY))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new(post.date)
                            .font(theme::regular(font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                });
                if post.locally_authored {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let more = icon_button(ui, Icon::More, "Post options");
                        egui::Popup::menu(&more).show(|ui| {
                            if ui.button("Edit post").clicked() {
                                action.edit_local_post = true;
                                ui.close();
                            }
                            if ui.button("Delete post").clicked() {
                                action.delete_local_post = true;
                                ui.close();
                            }
                        });
                    });
                }
            });
            ui.add_space(space::MD);
            if !post.title.is_empty() {
                ui.label(
                    RichText::new(post.title)
                        .font(theme::semibold(font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.add_space(space::SM);
            }
            ui.label(
                RichText::new(post.body)
                    .font(theme::regular(font_size::BODY))
                    .color(theme::fg1()),
            );
            if let Some(link) = post.link {
                ui.add_space(space::MD);
                if ui
                    .add(egui::Link::new(
                        RichText::new(link)
                            .font(theme::regular(font_size::BODY))
                            .color(theme::brand_fg()),
                    ))
                    .clicked()
                {
                    action.link = Some(link.into());
                }
            }
            ui.add_space(space::MD);
            // Pills render only for reactions that exist; the bar's trailing
            // add-reaction button is the sole way to react and is always kept.
            if let Some(icon) = draw_reaction_bar(ui, post.reactions) {
                action.reaction = Some(icon);
            }
            if post.source_reply_count > 0 || post.local_reply_count > 0 {
                ui.add_space(space::SM);
            }
            if post.source_reply_count > 0 {
                let row = ui.horizontal(|ui| {
                    ui.add_space(space::MD);
                    if ui
                        .add(egui::Link::new(
                            RichText::new(thread_reply_link_label(post.source_reply_count))
                                .font(theme::semibold(font_size::CAPTION))
                                .color(theme::brand_fg()),
                        ))
                        .clicked()
                    {
                        action.open_thread = true;
                    }
                    draw_reply_authors(ui, post.source_replies);
                });
                // Left accent bar tying the reply summary to the thread.
                let rect = row.response.rect;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())),
                    CornerRadius::same(1),
                    theme::brand_fg(),
                );
                if let Some(reply) = post.source_replies.first() {
                    draw_reply_preview(ui, reply);
                }
            }
            if post.local_reply_count > 0 {
                ui.label(
                    RichText::new(format!("{} local replies", post.local_reply_count))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            }
            ui.add_space(space::SM);
            let (divider, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(divider, 0.0, theme::stroke_subtle());
            ui.add_space(space::SM);
            ui.horizontal(|ui| {
                Avatar::new(&workspace().current_user, AvatarSize::Sm24).show(ui);
                if ui
                    .add(egui::Link::new(
                        RichText::new("Reply in thread")
                            .font(theme::regular(font_size::BODY))
                            .color(theme::fg2()),
                    ))
                    .clicked()
                {
                    action.open_thread = true;
                }
            });
        });
    let id = ui.id().with(("team_post_focus", post.key));
    focus_target(
        ui,
        card.response.rect,
        id,
        &format!("Post by {}, {}", post.author, post.date),
    );
    // Posts have no hover toolbar: Enter or Space on the focused card opens
    // its thread instead.
    action.nav_item = Some(NavItem {
        id,
        rect: card.response.rect,
        activate: None,
        secondary: None,
    });
    action
}

/// "from A, B" caption listing up to two distinct captured reply authors.
fn draw_reply_authors(ui: &mut egui::Ui, replies: &[SourceReply]) {
    let Some(first) = replies.first() else {
        return;
    };
    let second = replies
        .iter()
        .map(|reply| reply.author)
        .find(|author| *author != first.author);
    let caption = |text: &str| {
        RichText::new(text)
            .font(theme::regular(font_size::CAPTION))
            .color(theme::fg3())
    };
    ui.label(caption("from"));
    ui.label(caption(first.author));
    if let Some(second) = second {
        ui.label(caption("and"));
        ui.label(caption(second));
    }
}

/// One-line preview of a captured reply under the "Open N replies" summary.
fn draw_reply_preview(ui: &mut egui::Ui, reply: &SourceReply) {
    ui.horizontal(|ui| {
        ui.add_space(space::MD);
        Avatar::new(reply.author, AvatarSize::Xs20).show(ui);
        let galley = layout_truncated(
            ui,
            reply.body,
            theme::regular(font_size::CAPTION),
            theme::fg2(),
            ui.available_width(),
        );
        ui.label(galley);
    });
}

pub(super) fn source_reply_reaction_key(post_key: &str, reply: &SourceReply) -> String {
    format!("{post_key}:reply:{}", reply.key)
}

/// Stable identity of the thread's reply composer, so reply navigation can
/// return focus to it.
fn thread_composer_id() -> egui::Id {
    egui::Id::new("team_thread_reply_input")
}

#[cfg(test)]
pub(super) fn thread_composer_id_for_test() -> egui::Id {
    thread_composer_id()
}

/// Focus identity of a source reply, derived from its stable key under the
/// thread's own id.
fn source_reply_focus_id(thread_ui: egui::Id, reply: &SourceReply) -> egui::Id {
    thread_ui.with(("source_reply_focus", reply.key))
}

fn draw_source_thread_reply(
    ui: &mut egui::Ui,
    thread_ui: egui::Id,
    reply: &SourceReply,
    reactions: &ReactionSet,
) -> (Option<String>, NavItem) {
    let mut reaction_action = None;
    let card = egui::Frame::new()
        .fill(theme::surface())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.label(
                RichText::new(format!("{} · {}", reply.author, reply.date))
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
            ui.add_space(3.0);
            ui.label(
                RichText::new(reply.body)
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.add_space(5.0);
            if let Some(icon) = draw_reaction_bar(ui, reactions) {
                reaction_action = Some(icon);
            }
        });
    let id = source_reply_focus_id(thread_ui, reply);
    focus_target(
        ui,
        card.response.rect,
        id,
        &format!("Reply by {}, {}", reply.author, reply.date),
    );
    let item = NavItem {
        id,
        rect: card.response.rect,
        activate: None,
        secondary: None,
    };
    (reaction_action, item)
}

pub(super) fn draw_inline_post_thread(
    ui: &mut egui::Ui,
    state: &mut TeamsState,
    discussions: &mut ThreadStore,
    toast: &mut ToastState,
) {
    let Some(thread) = state.active_thread.clone() else {
        return;
    };
    if thread.channel != state.selected_channel {
        state.active_thread = None;
        state.thread_draft.clear();
        state.editing_reply = None;
        return;
    }
    // Esc on a focused reply belongs to reply navigation (back to the
    // composer), not to closing the thread.
    let thread_ui = ui.id();
    let reply_focused = thread
        .source_replies
        .iter()
        .any(|reply| ui.memory(|memory| memory.has_focus(source_reply_focus_id(thread_ui, reply))));
    if !reply_focused && take_escape(ui.ctx()) {
        if !state.cancel_editing_reply_for_thread(&thread) {
            state.active_thread = None;
            state.thread_draft.clear();
        }
        return;
    }

    let replies = state.thread_replies(discussions, &thread).to_vec();
    let source_reply_reactions = thread
        .source_replies
        .iter()
        .map(|reply| {
            state.reactions_for_post(
                discussions,
                &thread.key,
                // invariant: embedded seed data
                &ThreadTarget::source_reply(reply.key)
                    .expect("catalogued source reply keys must be valid"),
                reply.reactions,
            )
        })
        .collect::<Vec<_>>();
    let local_reply_reactions = replies
        .iter()
        .map(|reply| {
            // A zero id is rejected at load; if one ever appears, render the
            // reply without reactions so the row alignment is preserved.
            ThreadTarget::local_reply(reply.id()).map_or_else(ReactionSet::default, |target| {
                state.reactions_for_post(discussions, &thread.key, &target, &[])
            })
        })
        .collect::<Vec<_>>();
    let mut send_requested = false;
    let mut close_requested = false;
    let mut source_reply_reaction = None;
    let mut local_reply_reaction = None;
    let mut local_reply_edit_requested = None;
    let mut local_reply_edit_save_requested = None;
    let mut local_reply_edit_cancel_requested = None;
    let mut local_reply_delete_requested = None;
    egui::Frame::new()
        .fill(theme::surface_subtle())
        .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Thread").font(theme::semibold(font_size::BODY)).color(theme::fg1()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Close").clicked() {
                        close_requested = true;
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(RichText::new(&thread.title).font(theme::semibold(font_size::SUBTITLE)).color(theme::fg1()));
            ui.label(
                RichText::new(&thread.channel)
                    .font(theme::regular(font_size::CAPTION))
                    .color(theme::fg3()),
            );
            if thread.source_reply_count > 0 {
                let captured_count = thread.source_replies.len();
                let detail = if captured_count == 0 {
                    format!(
                        "The reference snapshot lists {} replies; their text was not captured locally.",
                        thread.source_reply_count
                    )
                } else if captured_count == thread.source_reply_count {
                    format!(
                        "{} reference replies are shown below. New replies are saved in this local profile.",
                        thread.source_reply_count
                    )
                } else {
                    format!(
                        "Showing {captured_count} captured reference replies from {}. New replies are saved in this local profile.",
                        thread.source_reply_count
                    )
                };
                ui.label(RichText::new(detail).font(theme::regular(font_size::CAPTION)).color(theme::fg3()));
            }
            ui.add_space(10.0);
            egui::ScrollArea::vertical()
                .max_height(240.0)
                .show(ui, |ui| {
                    if !thread.source_replies.is_empty() {
                        ui.label(
                            RichText::new("Reference replies")
                                .font(theme::semibold(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.add_space(6.0);
                    }
                    let mut nav = MessageNav::default();
                    for (index, reply) in thread.source_replies.iter().enumerate() {
                        if let Some(reactions) = source_reply_reactions.get(index) {
                            let (icon, item) =
                                draw_source_thread_reply(ui, thread_ui, reply, reactions);
                            nav.push(item);
                            if let Some(icon) = icon {
                                source_reply_reaction = Some((index, icon));
                            }
                        }
                        ui.add_space(7.0);
                    }
                    let entry = if ui.memory(|memory| memory.focused().is_none())
                        && ui.rect_contains_pointer(ui.clip_rect())
                    {
                        Entry::Anywhere
                    } else {
                        Entry::Never
                    };
                    // Replies have no toolbar, so Enter has nothing to open.
                    let _: NavOutcome = nav.handle(ui, thread_composer_id(), entry);
                    if !thread.source_replies.is_empty() && !replies.is_empty() {
                        ui.separator();
                        ui.add_space(7.0);
                    }
                    if !replies.is_empty() {
                        ui.label(
                            RichText::new("Your local replies")
                                .font(theme::semibold(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.add_space(6.0);
                    }
                    if thread.source_replies.is_empty() && replies.is_empty() {
                        ui.label(
                            RichText::new("No local replies in this thread yet.")
                                .font(theme::regular(font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    }
                    for (reply, reactions) in replies.iter().zip(&local_reply_reactions) {
                        ui.push_id(reply.id(), |ui| {
                            egui::Frame::new()
                                .fill(theme::surface_subtle())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(CornerRadius::same(radius::MD))
                                .inner_margin(egui::Margin::symmetric(12, 9))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new("You · Now").font(theme::regular(font_size::CAPTION)).color(theme::fg3()),
                                        );
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if !state.is_editing_local_reply(&thread, reply.id()) {
                                                let more = icon_button(ui, Icon::More, "Reply options");
                                                egui::Popup::menu(&more).show(|ui| {
                                                    if ui.button("Edit reply").clicked() {
                                                        local_reply_edit_requested = Some(reply.id());
                                                        ui.close();
                                                    }
                                                    if ui.button("Delete reply").clicked() {
                                                        local_reply_delete_requested = Some(reply.id());
                                                        ui.close();
                                                    }
                                                });
                                            }
                                        });
                                    });
                                    if state.is_editing_local_reply(&thread, reply.id()) {
                                        let focus_requested = state
                                            .take_editing_local_reply_focus(&thread, reply.id());
                                        let draft = state
                                            .editing_local_reply_draft_mut(&thread, reply.id());
                                        let input = draft.map(|draft| {
                                            ui.add(
                                                egui::TextEdit::multiline(draft)
                                                    .desired_rows(3)
                                                    .hint_text("Edit your reply"),
                                            )
                                        });
                                        if focus_requested && let Some(input) = &input {
                                            input.request_focus();
                                        }
                                        if input.is_some_and(|input| input.has_focus())
                                            && multiline_submit_pressed(ui)
                                        {
                                            local_reply_edit_save_requested = Some(reply.id());
                                        }
                                        ui.horizontal(|ui| {
                                            if ui.small_button("Cancel").clicked() {
                                                local_reply_edit_cancel_requested = Some(reply.id());
                                            }
                                            if ui.small_button("Save").clicked() {
                                                local_reply_edit_save_requested = Some(reply.id());
                                            }
                                        });
                                    } else {
                                        ui.label(RichText::new(reply.body()).font(theme::regular(font_size::CAPTION)).color(theme::fg1()));
                                    }
                                    ui.add_space(6.0);
                                    if let Some(icon) = draw_reaction_bar(ui, reactions) {
                                        local_reply_reaction = Some((reply.id(), icon));
                                    }
                                });
                        });
                        ui.add_space(7.0);
                    }
                });
            ui.add_space(10.0);
            let input = ui.add(
                egui::TextEdit::multiline(&mut state.thread_draft)
                    .id(thread_composer_id())
                    .desired_rows(3)
                    .hint_text("Reply in thread"),
            );
            if input.has_focus() && multiline_submit_pressed(ui) {
                send_requested = true;
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} to send", crate::ui::shortcuts::submit_chord()))
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Send reply").color(theme::fg_on_brand()))
                                .fill(theme::brand())
                                .corner_radius(CornerRadius::same(radius::MD)),
                        )
                        .clicked()
                    {
                        send_requested = true;
                    }
                });
            });
        });

    if send_requested {
        match state.submit_thread_reply(discussions) {
            Ok(()) => toast.show("Reply added to this local thread"),
            Err(error) => toast.show_error(error.message()),
        }
    }
    if let Some((index, icon)) = source_reply_reaction
        && let Some(reply) = thread.source_replies.get(index)
    {
        match state.toggle_post_reaction(
            discussions,
            &thread.key,
            // invariant: embedded seed data
            ThreadTarget::source_reply(reply.key)
                .expect("catalogued source reply keys must be valid"),
            reply.reactions,
            &icon,
        ) {
            Ok(true) => toast.show(format!("Added {icon} reaction")),
            Ok(false) => toast.show(format!("Removed {icon} reaction")),
            Err(error) => toast.show(error.to_string()),
        }
    }
    if let Some((reply_id, icon)) = local_reply_reaction {
        if let Some(target) = ThreadTarget::local_reply(reply_id) {
            match state.toggle_post_reaction(discussions, &thread.key, target, &[], &icon) {
                Ok(true) => toast.show(format!("Added {icon} reaction")),
                Ok(false) => toast.show(format!("Removed {icon} reaction")),
                Err(error) => toast.show(error.to_string()),
            }
        } else {
            toast.show_error("Couldn't update that reaction.");
        }
    }
    if let Some(reply_id) = local_reply_edit_requested
        && let Some(reply) = replies.iter().find(|reply| reply.id() == reply_id)
    {
        state.begin_editing_local_reply(&thread, reply);
    }
    if let Some(reply_id) = local_reply_delete_requested {
        match state.delete_local_reply(discussions, &thread, reply_id) {
            Ok(()) => toast.show("Local reply deleted"),
            Err(error) => toast.show(error.to_string()),
        }
    } else if let Some(reply_id) = local_reply_edit_cancel_requested {
        state.cancel_editing_local_reply(&thread, reply_id);
    } else if let Some(reply_id) = local_reply_edit_save_requested {
        match state.save_editing_local_reply(discussions, &thread, reply_id) {
            Ok(()) => toast.show("Local reply updated"),
            Err(error) => toast.show(error.to_string()),
        }
    }
    if close_requested {
        state.active_thread = None;
        state.thread_draft.clear();
        state.editing_reply = None;
    }
}
