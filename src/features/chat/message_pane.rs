use super::ChatState;
use super::persistence::MAX_MESSAGE_TEXT_BYTES;
use super::remote::{RemoteChatHistoryState, RemoteChatReactionView, RemoteMessageActionsView};
use crate::data::shared_files::shared_file_for_message_attachment;
use crate::data::workspace::workspace;
use crate::features::chat::tabs::{
    SharedTabAction, draw_info_tab, draw_notes_tab, draw_recap_tab, draw_shared_tab,
    draw_storyline_tab,
};
use crate::integrations::RemoteChatMessageImportance;
use crate::shared::model::{ChatTab, Message, MessageAttachment};
use crate::shared::reaction::ReactionKind;
use crate::ui::icon::{Icon, icon_button, icon_button_selected};
use crate::ui::link_preview::LinkPreview;
use crate::ui::message_nav::{Entry, MessageNav, NavOutcome};
use crate::ui::message_view::{MessageAction, MessageOptions, draw_message_tracked};
use crate::ui::shortcuts::{singleline_submit_pressed, take_escape};
use crate::ui::tabs::tab_strip_clicked;
use crate::ui::theme;
use crate::ui::toast::{ToastState, UndoKind};
use eframe::egui::{self, Align, Layout, RichText, Stroke};

use super::{
    ConversationSelection, MessageEditor, SendOutcome, TimelineAction, truncate_to_byte_limit,
};

/// Stable identity of the local composer text field, so message navigation
/// can return focus to it.
pub(crate) fn local_composer_id() -> egui::Id {
    egui::Id::new("local_chat_composer_input")
}

impl ChatState {
    /// Whether the selected conversation's composer has no draft text.
    fn selected_draft_is_empty(&mut self) -> bool {
        if let ConversationSelection::Remote(index) = self.selected {
            return self
                .remote_chats
                .get_mut(index)
                .is_none_or(|conversation| conversation.draft_mut().is_empty());
        }
        self.selected_composer_mut()
            .is_none_or(|composer| composer.draft.is_empty())
    }

    pub(super) fn draw_tabs_inline(&mut self, ui: &mut egui::Ui) {
        const TABS: [(ChatTab, &str); 5] = [
            (ChatTab::Chat, "Chat"),
            (ChatTab::Shared, "Shared"),
            (ChatTab::Notes, "Notes"),
            (ChatTab::Recap, "Recap"),
            (ChatTab::Storyline, "Storyline"),
        ];
        const LABELS: [&str; 5] = [TABS[0].1, TABS[1].1, TABS[2].1, TABS[3].1, TABS[4].1];
        // Info is opened from the add-tab menu and has no strip entry.
        let selected = TABS
            .iter()
            .position(|(tab, _)| *tab == self.tab)
            .unwrap_or(usize::MAX);
        if let Some(index) = tab_strip_clicked(ui, &LABELS, selected) {
            self.tab = TABS[index].0;
        }
        let add_tab = icon_button(ui, Icon::Add, "Add a tab");
        egui::Popup::menu(&add_tab).show(|ui| {
            if ui.button("Conversation info").clicked() {
                self.tab = ChatTab::Info;
                ui.close();
            }
            if ui.button("Open recap").clicked() {
                self.tab = ChatTab::Recap;
                ui.close();
            }
        });
    }

    pub(super) fn draw_message_tab(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        let remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            _ => None,
        };
        let remote_history_state =
            remote_index.and_then(|index| self.remote_chats.history_state(index));
        let mut send_requested = false;
        let mut composer_changed = false;
        let composer_frame = || {
            egui::Frame::new()
                .fill(theme::canvas())
                .inner_margin(egui::Margin::symmetric(16, 12))
        };
        let composer_box = || {
            egui::Frame::new()
                .fill(theme::surface())
                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                .corner_radius(egui::CornerRadius::same(theme::radius::LG))
                .inner_margin(egui::Margin::symmetric(12, 8))
        };
        let caption = |text: &str| {
            RichText::new(text.to_owned())
                .font(theme::regular(theme::font_size::CAPTION))
                .color(theme::fg3())
        };
        if let Some(remote_index) = remote_index {
            let mut remote_send_requested = false;
            let mut open_people_requested = false;
            let mut attach_pick_requested = false;
            let mut cancel_upload_requested = false;
            let file_attach_offered = self.file_attach_offered();
            let people_open = self.remote_chats.has_members_panel(remote_index);
            // No fixed height: the panel grows with its content, so the box
            // and its hint are never clipped at the bottom edge.
            egui::TopBottomPanel::bottom("message_composer")
                .frame(composer_frame())
                .show_inside(ui, |ui| {
                    composer_box().show(ui, |ui| {
                        let Some(conversation) = self.remote_chats.get_mut(remote_index) else {
                            return;
                        };
                        let quote_contexts: Vec<(String, String)> = conversation
                            .quote_reply_contexts()
                            .iter()
                            .map(|reply| (reply.name.clone(), reply.text.clone()))
                            .collect();
                        if let Some(notice) = conversation.take_quote_notice() {
                            toast.show(notice);
                        }
                        let quote_active = conversation.has_quote_reply();
                        let editing_original = conversation.editing_original_text().map(str::to_owned);
                        if editing_original.is_some() {
                            egui::Frame::new()
                                .fill(theme::brand_tint())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(egui::CornerRadius::same(theme::radius::SM))
                                .inner_margin(egui::Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(caption("Editing message"));
                                        if ui.small_button("Cancel edit").clicked() {
                                            let _ = conversation.cancel_edit();
                                        }
                                    });
                                });
                            ui.add_space(4.0);
                        }
                        if !quote_contexts.is_empty() {
                            let removable = !conversation.has_pending_send();
                            let mut remove_at = None;
                            egui::Frame::new()
                                .fill(theme::brand_tint())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(egui::CornerRadius::same(theme::radius::SM))
                                .inner_margin(egui::Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(caption(&format!(
                                            "Replying to {} message{}",
                                            quote_contexts.len(),
                                            if quote_contexts.len() == 1 { "" } else { "s" }
                                        )));
                                        if removable
                                            && ui.small_button("Cancel quote").clicked()
                                        {
                                            let _ = conversation.cancel_quote_reply();
                                        }
                                    });
                                    for (position, (name, text)) in
                                        quote_contexts.iter().enumerate()
                                    {
                                        ui.horizontal_wrapped(|ui| {
                                            let preview: String = text.chars().take(60).collect();
                                            let ellipsis =
                                                if text.chars().count() > 60 { "…" } else { "" };
                                            ui.label(
                                                RichText::new(format!(
                                                    "{name}: {preview}{ellipsis}"
                                                ))
                                                .font(theme::regular(theme::font_size::CAPTION))
                                                .color(theme::fg1()),
                                            );
                                            if removable
                                                && ui
                                                    .small_button("Remove")
                                                    .on_hover_text("Remove from quote")
                                                    .clicked()
                                            {
                                                remove_at = Some(position);
                                            }
                                        });
                                    }
                                });
                            if let Some(position) = remove_at {
                                let _ = conversation.remove_quote_source(position);
                            }
                            ui.add_space(4.0);
                        }
                        if !conversation.has_pending_send()
                            && let Some(view) = conversation.attachment_view()
                        {
                            use super::remote::AttachmentView;
                            let mut accept = false;
                            let mut remove = false;
                            let mut cancel_upload = false;
                            egui::Frame::new()
                                .fill(theme::brand_tint())
                                .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                .corner_radius(egui::CornerRadius::same(theme::radius::SM))
                                .inner_margin(egui::Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        match &view {
                                            AttachmentView::Uploading { name, size, sent } => {
                                                ui.spinner();
                                                let percent = if *size == 0 {
                                                    0
                                                } else {
                                                    (u128::from(*sent) * 100 / u128::from(*size)) as u64
                                                };
                                                ui.label(caption(&format!(
                                                    "Uploading {name} ({} of {}, {percent}%)",
                                                    super::attachments::format_size(*sent),
                                                    super::attachments::format_size(*size)
                                                )));
                                                cancel_upload = ui.small_button("Cancel").clicked();
                                            }
                                            AttachmentView::Ready { name, size } => {
                                                ui.label(caption(&format!(
                                                    "Attached: {name} ({})",
                                                    super::attachments::format_size(*size)
                                                )));
                                                remove = ui.small_button("Remove").clicked();
                                            }
                                            AttachmentView::Recovered { name, size } => {
                                                ui.label(caption(&format!(
                                                    "{name} ({}) may already be in OneDrive.",
                                                    super::attachments::format_size(*size)
                                                )));
                                                accept = ui.small_button("Attach").clicked();
                                                remove = ui.small_button("Remove").clicked();
                                            }
                                            AttachmentView::Conflict { name } => {
                                                ui.label(caption(&format!(
                                                    "{name}: a file with that name is already in your Teams chat files. Rename it and pick it again."
                                                )));
                                                remove = ui.small_button("Dismiss").clicked();
                                            }
                                            AttachmentView::Failed { name, reason } => {
                                                ui.label(caption(&format!("{name}: {reason}")));
                                                remove = ui.small_button("Dismiss").clicked();
                                            }
                                        }
                                    });
                                });
                            if accept {
                                let _ = conversation.accept_recovered_attachment();
                            }
                            if remove {
                                let _ = conversation.remove_attachment();
                            }
                            if cancel_upload {
                                cancel_upload_requested = true;
                            }
                            ui.add_space(4.0);
                        }
                        if conversation.has_pending_send() {
                            let (headline, detail) = if let Some(status) =
                                conversation.mutation_status()
                            {
                                (
                                    status,
                                    "The conversation updates after the next remote sync.",
                                )
                            } else if conversation.is_awaiting_reconciliation() {
                                (
                                    "Checking Microsoft 365 before retrying…",
                                    "The original draft is protected until this chat refresh confirms or rules out delivery.",
                                )
                            } else {
                                (
                                    "Sending to Microsoft 365…",
                                    "The message will appear after the next remote sync.",
                                )
                            };
                            ui.horizontal(|ui| {
                                let mut importance = conversation.composer_importance();
                                draw_remote_importance_selector(
                                    ui,
                                    remote_index,
                                    &mut importance,
                                    false,
                                );
                                ui.spinner();
                                ui.label(caption(headline));
                            });
                            ui.label(caption(detail));
                        } else {
                            let has_text = !conversation.draft_mut().trim().is_empty()
                                || conversation.has_sendable_attachment();
                            let editing = editing_original.is_some();
                            // Formatting is markdown-lite, which sends and edits
                            // interpret but quote replies (plain text) do not.
                            if !quote_active {
                                super::remote_composer::draw_format_toolbar(
                                    ui,
                                    conversation.draft_mut(),
                                );
                                super::remote_composer::draw_preview(ui, conversation.draft_mut());
                            }
                            // Keys the picker uses are taken before the field sees
                            // them, so Enter picks a member instead of sending.
                            let picker = super::mention_picker::begin(ui.ctx(), conversation);
                            ui.horizontal(|ui| {
                                let input_width = (ui.available_width() - 186.0).max(120.0);
                                let input = ui.add_sized(
                                    [input_width, 32.0],
                                    egui::TextEdit::singleline(conversation.draft_mut())
                                        .id(super::remote_composer::remote_composer_id())
                                        .hint_text(if editing {
                                            "Edit your message"
                                        } else {
                                            "Type a Microsoft 365 message"
                                        })
                                        .frame(false),
                                );
                                if let Some(frame) = &picker
                                    && super::mention_picker::show(
                                        ui,
                                        conversation,
                                        &input,
                                        frame,
                                        people_open,
                                    )
                                {
                                    open_people_requested = true;
                                }
                                if singleline_submit_pressed(&input, ui) {
                                    remote_send_requested = true;
                                }
                                if icon_button_selected(
                                    ui,
                                    Icon::Send,
                                    has_text,
                                    if editing { "Save edit" } else { "Send message" },
                                )
                                .clicked()
                                {
                                    remote_send_requested = true;
                                }
                                if !editing
                                    && !quote_active
                                    && ui
                                        .add_enabled_ui(file_attach_offered, |ui| {
                                            icon_button(ui, Icon::Attach, "Attach a file (up to 100 MiB)")
                                        })
                                        .inner
                                        .clicked()
                                {
                                    attach_pick_requested = true;
                                }
                                let mut importance = conversation.composer_importance();
                                let importance_enabled = !quote_active && !editing;
                                draw_remote_importance_selector(
                                    ui,
                                    remote_index,
                                    &mut importance,
                                    importance_enabled,
                                );
                                if importance_enabled {
                                    conversation.set_composer_importance(importance);
                                }
                            });
                            ui.label(caption(if editing {
                                super::remote_composer::HINT_EDIT
                            } else if quote_active {
                                super::remote_composer::HINT_QUOTE
                            } else {
                                super::remote_composer::HINT_SEND
                            }));
                        }
                    });
                });
            if open_people_requested {
                self.toggle_remote_chat_members(remote_index);
            }
            if cancel_upload_requested {
                self.cancel_remote_upload(remote_index);
            }
            if attach_pick_requested {
                self.pick_remote_attachment(remote_index, toast);
            }
            if remote_send_requested && let Err(error) = self.queue_remote_message(remote_index) {
                toast.show_error(error.user_message());
            }
        } else {
            egui::TopBottomPanel::bottom("message_composer")
                .frame(composer_frame())
                .show_inside(ui, |ui| {
                    composer_box().show(ui, |ui| {
                        let Some(composer) = self.selected_composer_mut() else {
                            return;
                        };
                        let has_text = !composer.draft.trim().is_empty();
                        ui.horizontal(|ui| {
                            // Six 32px icon buttons plus their spacing trail the input.
                            let input_width = (ui.available_width() - 6.0 * 40.0).max(120.0);
                            let input = ui.add_sized(
                                [input_width, 32.0],
                                egui::TextEdit::singleline(&mut composer.draft)
                                    .id(local_composer_id())
                                    .hint_text("Type a message")
                                    .frame(false),
                            );
                            composer_changed |= input.changed();
                            if singleline_submit_pressed(&input, ui) {
                                send_requested = true;
                            }
                            if icon_button(ui, Icon::TextFormat, "Format").clicked() {
                                toast.show(
                                    "Text formatting is not available in this local preview.",
                                );
                            }
                            if icon_button(ui, Icon::Emoji, "Emoji").clicked() {
                                composer.draft.push('☺');
                                composer_changed = true;
                            }
                            if icon_button_selected(
                                ui,
                                Icon::Attach,
                                composer.attachment_picker_open,
                                "Attach",
                            )
                            .clicked()
                            {
                                composer.attachment_picker_open = !composer.attachment_picker_open;
                            }
                            if icon_button(ui, Icon::Sticker, "Loop components").clicked() {
                                toast.show(
                                    "Loop components are not available in this local preview.",
                                );
                            }
                            if icon_button(ui, Icon::Add, "Actions and apps").clicked() {
                                toast.show(
                                    "Actions and apps are not available in this local preview.",
                                );
                            }
                            if icon_button_selected(
                                ui,
                                Icon::Send,
                                has_text || composer.pending_attachment.is_some(),
                                "Send message",
                            )
                            .clicked()
                            {
                                send_requested = true;
                            }
                        });
                        if truncate_to_byte_limit(&mut composer.draft, MAX_MESSAGE_TEXT_BYTES) {
                            composer_changed = true;
                            toast.show(
                                "Message drafts are limited to 64 KiB in this local profile.",
                            );
                        }
                        if let Some(attachment) = composer.pending_attachment {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("Attached: {}", attachment.label()))
                                        .font(theme::regular(theme::font_size::CAPTION))
                                        .color(theme::brand_fg()),
                                );
                                if ui.small_button("Remove").clicked() {
                                    composer.pending_attachment = None;
                                    composer_changed = true;
                                }
                            });
                        }
                        if composer.attachment_picker_open {
                            if take_escape(ui.ctx()) {
                                composer.attachment_picker_open = false;
                            } else {
                                ui.horizontal(|ui| {
                                    ui.label(caption("Attach preview asset:"));
                                    for attachment in [
                                        MessageAttachment::LayoutMockup,
                                        MessageAttachment::Spreadsheet,
                                    ] {
                                        if ui.small_button(attachment.label()).clicked() {
                                            composer.pending_attachment = Some(attachment);
                                            composer.attachment_picker_open = false;
                                            composer_changed = true;
                                            toast.show(format!("{} attached", attachment.label()));
                                        }
                                    }
                                });
                            }
                        }
                    });
                });
            if composer_changed {
                self.mark_persistence_dirty();
            }
            if send_requested && let SendOutcome::Rejected(error) = self.send() {
                toast.show_error(error.user_message());
            }
        }
        // The supplied chat reference opens at the start of the conversation.
        // Reserve automatic bottom scrolling for a newly sent local message,
        // where following the latest content is the expected behavior.
        let scroll_to_bottom = std::mem::take(&mut self.scroll_to_bottom_requested);
        let composer_id = if remote_index.is_some() {
            super::remote_composer::remote_composer_id()
        } else {
            local_composer_id()
        };
        let composer_draft_empty = self.selected_draft_is_empty();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add_space(5.0);
                match remote_history_state {
                    Some(RemoteChatHistoryState::Loading) => {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(theme::space::XL));
                            ui.label(
                                RichText::new("Refreshing recent Microsoft 365 messages…")
                                    .font(theme::regular(theme::font_size::CAPTION))
                                    .color(theme::fg3()),
                            );
                        });
                        ui.add_space(theme::space::SM);
                    }
                    Some(RemoteChatHistoryState::AccessDenied) => {
                        ui.label(
                            RichText::new(
                                "Microsoft 365 did not allow this chat's message history. Check your chat access and reconnect to try again.",
                            )
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                        );
                        ui.add_space(theme::space::SM);
                    }
                    Some(RemoteChatHistoryState::Refreshing) => {
                        ui.label(
                            RichText::new("Checking for new messages…")
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.add_space(theme::space::SM);
                    }
                    Some(RemoteChatHistoryState::Stale) => {
                        ui.label(
                            RichText::new("Couldn't refresh. Showing earlier messages.")
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                        ui.add_space(theme::space::SM);
                    }
                    Some(RemoteChatHistoryState::NotRequested | RemoteChatHistoryState::Ready)
                    | None => {}
                }
                if self.selected_messages().is_empty() {
                    ui.add_space(72.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("No messages yet")
                                .font(theme::semibold(theme::font_size::SUBTITLE))
                                .color(theme::fg1()),
                        );
                        ui.label(
                            RichText::new("Start the conversation with a message or attachment.")
                                .font(theme::regular(theme::font_size::SMALL))
                                .color(theme::fg3()),
                        );
                    });
                } else {
                    let search_focus_taken = std::cell::Cell::new(false);
                    let (selected_action, quote_requested, row_action) = egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(16, 0))
                        .show(ui, |ui| {
                            let messages = self.selected_messages();
                            let timeline = self.timeline_for(self.selected);
                            let day_label = &workspace().chat.day_label;
                            let mut selected_action = None;
                            let mut quote_requested = None;
                            let mut row_action = None;
                            let mut nav = MessageNav::default();
                            let capabilities = self.effective_remote_capabilities();
                            for (index, message) in messages.iter().enumerate() {
                                if message.day_divider_before {
                                    draw_day_divider(ui, day_label);
                                }
                                // The first message, the first one after a day
                                // divider, and any change of sender keep a header.
                                let show_header = message.day_divider_before
                                    || index
                                        .checked_sub(1)
                                        .and_then(|previous| messages.get(previous))
                                        .is_none_or(|previous| {
                                            previous.own != message.own
                                                || previous.name != message.name
                                        });
                                let local_message_id =
                                    timeline.and_then(|timeline| timeline.local_id_at(index));
                                let options = MessageOptions {
                                    show_header,
                                    editable: local_message_id.is_some(),
                                    reaction_pills_enabled: remote_index.is_none(),
                                    hover_toolbar_enabled: remote_index.is_none(),
                                };
                                // Local messages key on their local id, remote
                                // ones on a hash of the opaque provider target;
                                // the list position is only the last resort.
                                let remote_key = remote_index.and_then(|remote_index| {
                                    self.remote_chats.message_focus_key(remote_index, index)
                                });
                                let (drawn, nav_item) = draw_message_tracked(
                                    ui,
                                    (
                                        self.selected,
                                        local_message_id,
                                        remote_key,
                                        (local_message_id.is_none() && remote_key.is_none())
                                            .then_some(index),
                                    ),
                                    message,
                                    options,
                                );
                                // A Microsoft 365 search hit scrolls to its row and
                                // takes keyboard focus once, when it is drawn.
                                if let (Some(target), Some(key)) =
                                    (self.search_focus.as_ref(), remote_key)
                                    && target.stable_key() == key
                                {
                                    ui.scroll_to_rect(nav_item.rect, Some(egui::Align::Center));
                                    ui.memory_mut(|memory| memory.request_focus(nav_item.id));
                                    search_focus_taken.set(true);
                                }
                                nav.push(nav_item);
                                if let Some(action) = drawn {
                                    selected_action = Some(match (action, local_message_id) {
                                        (MessageAction::Edit, Some(id)) => TimelineAction::Edit(id),
                                        (MessageAction::Delete, Some(id)) => {
                                            TimelineAction::Delete(id)
                                        }
                                        (action, _) => TimelineAction::Message { index, action },
                                    });
                                }
                                if let Some(remote_index) = remote_index
                                    && let Some(importance) = self
                                        .remote_chats
                                        .message_importance(remote_index, index)
                                {
                                    draw_remote_importance_badge(ui, message, importance);
                                }
                                // Remote reaction controls are drawn by the
                                // narrow feature-owned view for every provider
                                // row. The shared local widget stays unaware
                                // of opaque Graph targets and cannot expose
                                // unreconcilable reaction actions.
                                if let Some(remote_index) = remote_index
                                    && let Some(reactions) =
                                        self.remote_chats.reaction_views(remote_index, index)
                                    && let Some(reaction) =
                                        draw_remote_reaction_controls(ui, message, &reactions)
                                {
                                    selected_action = Some(TimelineAction::Message {
                                        index,
                                        action: MessageAction::ToggleReaction(reaction),
                                    });
                                }
                                // Quote replies stay in this remote-only
                                // presentation path rather than changing the
                                // generic local message action surface. The
                                // opaque route remains inside RemoteChatState.
                                if let Some(remote_index) = remote_index
                                    && let Some(quote) = self
                                        .remote_chats
                                        .quote_reply_view(remote_index, index)
                                    && draw_remote_quote_reply_control(ui, message, quote)
                                {
                                    quote_requested = Some(index);
                                }
                                // Edit and Delete are offered only for the
                                // user's own, targeted rows of a Ready history.
                                if let Some(remote_index) = remote_index
                                    && let Some(view) = self.remote_chats.message_actions_view(
                                        remote_index,
                                        index,
                                        capabilities,
                                    )
                                    && let Some(action) =
                                        draw_remote_message_actions(ui, message, view)
                                {
                                    row_action = Some((index, action));
                                }
                            }
                            let composer_focused =
                                ui.memory(|memory| memory.has_focus(composer_id));
                            let nothing_focused = ui.memory(|memory| memory.focused().is_none());
                            let entry = if composer_focused {
                                if composer_draft_empty {
                                    Entry::FromComposer
                                } else {
                                    Entry::Never
                                }
                            } else if nothing_focused && ui.rect_contains_pointer(ui.max_rect()) {
                                Entry::Anywhere
                            } else {
                                Entry::Never
                            };
                            // Enter and Space open a row's toolbar; rows
                            // without one have nothing further to open.
                            let _: NavOutcome = nav.handle(ui, composer_id, entry);
                            (selected_action, quote_requested, row_action)
                        })
                        .inner;
                    if search_focus_taken.get() {
                        self.search_focus = None;
                    }
                    if let Some(action) = selected_action {
                        match action {
                            TimelineAction::Message { index, action } => {
                                self.handle_message_action(index, action, toast)
                            }
                            TimelineAction::Edit(local_message_id) => {
                                self.open_local_message_editor(local_message_id)
                            }
                            TimelineAction::Delete(local_message_id) => {
                                match self.delete_local_message(local_message_id) {
                                    Ok(()) => toast.show_undo("Local message deleted", UndoKind::ChatMessage),
                                    Err(error) => toast.show_error(error.user_message()),
                                }
                            }
                        }
                    }
                    if let (Some(remote_index), Some(message_index)) =
                        (remote_index, quote_requested)
                        && let Err(error) =
                            self.toggle_remote_quote_source(remote_index, message_index)
                    {
                        toast.show_error(error.user_message());
                    }
                    if let (Some(remote_index), Some((message_index, action))) =
                        (remote_index, row_action)
                    {
                        let outcome = match action {
                            RemoteRowAction::Edit => {
                                self.begin_remote_edit(remote_index, message_index)
                            }
                            RemoteRowAction::Delete => {
                                self.request_remote_delete(remote_index, message_index)
                            }
                        };
                        if let Err(error) = outcome {
                            toast.show_error(error.user_message());
                        }
                    }
                }
                ui.add_space(8.0);
                if scroll_to_bottom {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
    }

    pub(super) fn draw_secondary_tab(
        &mut self,
        ui: &mut egui::Ui,
        selected_name: &str,
        toast: &mut ToastState,
    ) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(22.0);
                match self.tab {
                    ChatTab::Chat => {}
                    ChatTab::Shared => {
                        match draw_shared_tab(ui, selected_name, self.selected_messages()) {
                            Some(SharedTabAction::PreviewAttachment(attachment)) => {
                                self.open_attachment_preview(attachment);
                            }
                            Some(SharedTabAction::SelectLink(link)) => {
                                let _ = self.open_link_preview(&link);
                            }
                            None => {}
                        }
                    }
                    ChatTab::Notes => {
                        let notes_changed = self
                            .selected_notes_mut()
                            .is_some_and(|notes| draw_notes_tab(ui, notes));
                        if notes_changed {
                            self.mark_persistence_dirty();
                        }
                    }
                    ChatTab::Recap => draw_recap_tab(ui, self.selected_messages()),
                    ChatTab::Storyline => {
                        if let Some((index, action)) = draw_storyline_tab(
                            ui,
                            self.selected_messages(),
                            if matches!(self.selected, ConversationSelection::Remote(_)) {
                                super::remote_composer::remote_composer_id()
                            } else {
                                local_composer_id()
                            },
                        ) {
                            self.handle_message_action(index, action, toast);
                        }
                    }
                    ChatTab::Info => draw_info_tab(ui, selected_name, self.selected_messages()),
                }
            });
    }

    pub(super) fn draw_shared_preview(&mut self, ctx: &egui::Context) {
        let Some(attachment) = self.shared_preview else {
            return;
        };
        if self.link_preview.is_none() && take_escape(ctx) {
            self.shared_preview = None;
            return;
        }

        let Some(file) = shared_file_for_message_attachment(attachment) else {
            self.shared_preview = None;
            return;
        };
        let mut open = true;
        egui::Window::new("Shared preview")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(470.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(file.name)
                        .font(theme::semibold(theme::font_size::SUBTITLE))
                        .color(theme::fg1()),
                );
                ui.label(
                    RichText::new("Previewed from this local chat session")
                        .font(theme::regular(theme::font_size::SMALL))
                        .color(theme::fg3()),
                );
                ui.add_space(12.0);
                ui.add(
                    egui::Image::from_bytes(file.asset.byte_uri, file.asset.bytes)
                        .fit_to_exact_size(egui::vec2(420.0, 247.0))
                        .corner_radius(6.0),
                );
            });
        if !open {
            self.shared_preview = None;
        }
    }

    pub(super) fn open_attachment_preview(&mut self, attachment: MessageAttachment) {
        self.shared_preview = Some(attachment);
    }

    fn open_link_preview(&mut self, text: &str) -> bool {
        let preview = LinkPreview::from_text(text);
        if let Some(preview) = preview {
            self.link_preview = Some(preview);
            true
        } else {
            false
        }
    }

    pub(super) fn handle_message_action(
        &mut self,
        message_index: usize,
        action: MessageAction,
        toast: &mut ToastState,
    ) {
        if let ConversationSelection::Remote(remote_index) = self.selected {
            match action {
                MessageAction::ToggleReaction(reaction) => {
                    let _ = self.queue_remote_chat_reaction(remote_index, message_index, reaction);
                }
                MessageAction::OpenRichLink { run } => {
                    let destination = self
                        .remote_chats
                        .get(remote_index)
                        .and_then(|conversation| conversation.messages().get(message_index))
                        .and_then(|message| message.rich.as_ref())
                        .and_then(|runs| runs.get(run))
                        .and_then(|run| match run {
                            crate::integrations::Inline::Link { url, .. } => {
                                Some(url.as_str().to_owned())
                            }
                            crate::integrations::Inline::Text { .. }
                            | crate::integrations::Inline::Mention { .. } => None,
                        });
                    if let Some(destination) = destination {
                        self.pending_link_opens.push(destination);
                    }
                }
                _ => toast.show(
                    "Microsoft 365 text sending and bounded reactions are available, but other message mutations are not yet enabled.",
                ),
            }
            return;
        }
        match action {
            MessageAction::PreviewAttachment(attachment) => {
                self.open_attachment_preview(attachment)
            }
            MessageAction::SelectLink => {
                let link_text = self
                    .selected_messages()
                    .get(message_index)
                    .map(|message| message.text.clone());
                if !link_text.is_some_and(|text| self.open_link_preview(&text)) {
                    toast.show_error("No local link details are available for this message.");
                }
            }
            MessageAction::ToggleReaction(kind) => {
                if let Some(added) = self.toggle_message_reaction(message_index, kind) {
                    toast.show(format!(
                        "{} {} reaction",
                        if added { "Added" } else { "Removed" },
                        kind.label()
                    ));
                }
            }
            // Edit and delete are routed as `TimelineAction`s by the timeline,
            // which owns the stable local message ID.
            MessageAction::Edit | MessageAction::Delete => {}
            // Only formatted remote messages carry link runs.
            MessageAction::OpenRichLink { .. } => {}
        }
    }

    pub(super) fn open_local_message_editor(&mut self, local_message_id: u64) {
        let selected = self.selected;
        let Some(chat_id) = self.selected_chat_id() else {
            return;
        };
        let Some(message) = self
            .timeline_for(selected)
            .and_then(|timeline| {
                timeline
                    .index_for_local_id(local_message_id)
                    .and_then(|index| timeline.messages.get(index))
            })
            .filter(|message| message.own)
        else {
            return;
        };
        self.editing_message = Some(MessageEditor {
            chat_id,
            local_message_id,
            draft: message.text.clone(),
            focus_requested: true,
        });
    }
}

/// Renders the remote-only typed priority selector. A pending or reconciling
/// submission keeps the captured priority visible but disables mutation until
/// its delivery outcome is known.
fn draw_remote_importance_selector(
    ui: &mut egui::Ui,
    remote_index: usize,
    importance: &mut RemoteChatMessageImportance,
    enabled: bool,
) {
    ui.add_enabled_ui(enabled, |ui| {
        let response = egui::ComboBox::from_id_salt(("remote_message_importance", remote_index))
            .selected_text(format!("Priority: {}", importance.label()))
            .width(108.0)
            .show_ui(ui, |ui| {
                for candidate in RemoteChatMessageImportance::ALL {
                    ui.selectable_value(importance, candidate, candidate.label());
                }
            });
        response.response.on_hover_text("Message priority");
    });
}

/// Shows the provider-normalized non-default priority next to the safe local
/// presentation row. The badge never exposes provider IDs or a mutation.
fn draw_remote_importance_badge(
    ui: &mut egui::Ui,
    message: &Message,
    importance: RemoteChatMessageImportance,
) {
    let Some(label) = importance.badge_label() else {
        return;
    };
    let draw = |ui: &mut egui::Ui| {
        egui::Frame::new()
            .fill(theme::brand_tint())
            .stroke(Stroke::new(1.0_f32, theme::brand_fg()))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(8, 3))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!("{label} priority"))
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg1()),
                );
            });
    };
    if message.own {
        ui.with_layout(Layout::right_to_left(Align::Min), draw);
    } else {
        ui.horizontal(|ui| {
            // Align the badge with the incoming message column rather than
            // with its avatar.
            ui.add_space(40.0);
            draw(ui);
        });
    }
}

/// Renders the bounded remote-chat reaction palette. It is intentionally
/// separate from the shared local message toolbar: this view has no access to
/// opaque Graph chat/message targets. Each control carries only a typed kind,
/// while the feature rechecks the target-bound projection before staging it.
fn draw_remote_reaction_controls(
    ui: &mut egui::Ui,
    message: &Message,
    reactions: &[RemoteChatReactionView],
) -> Option<ReactionKind> {
    let mut requested = None;
    let mut draw = |ui: &mut egui::Ui| {
        ui.horizontal_wrapped(|ui| {
            for reaction in reactions {
                // A malformed/deleted provider row remains presentation-only,
                // but provider-projected counts remain useful context. Omit
                // absent kinds on such a row so it cannot look writable.
                if !reaction.can_react && reaction.count == 0 {
                    continue;
                }
                let label = if !reaction.can_react {
                    "This Microsoft 365 message cannot receive reactions".into()
                } else if reaction.pending {
                    format!(
                        "Waiting for Microsoft 365 to confirm your {} reaction",
                        reaction.reaction.label()
                    )
                } else if reaction.selected {
                    format!("Remove your {} reaction", reaction.reaction.label())
                } else {
                    format!("Add a {} reaction", reaction.reaction.label())
                };
                let response = ui.add_enabled(
                    reaction.can_react && !reaction.pending && requested.is_none(),
                    egui::Button::new(
                        RichText::new(if reaction.count == 0 {
                            reaction.reaction.icon().into()
                        } else {
                            format!("{} {}", reaction.reaction.icon(), reaction.count)
                        })
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(if reaction.selected {
                            theme::brand_fg()
                        } else {
                            theme::fg1()
                        }),
                    )
                    .fill(if reaction.selected {
                        theme::brand_tint()
                    } else {
                        theme::surface()
                    })
                    .stroke(Stroke::new(
                        1.0_f32,
                        if reaction.selected {
                            theme::brand_fg()
                        } else {
                            theme::stroke_subtle()
                        },
                    ))
                    .corner_radius(egui::CornerRadius::same(10)),
                );
                if response.on_hover_text(label).clicked() {
                    requested = Some(reaction.reaction);
                }
            }
        });
    };
    if message.own {
        ui.with_layout(Layout::right_to_left(Align::Min), draw);
    } else {
        ui.horizontal(|ui| {
            // Align the pill with the incoming message column rather than
            // with its avatar.
            ui.add_space(40.0);
            draw(ui);
        });
    }
    requested
}

#[derive(Clone, Copy)]
enum RemoteRowAction {
    Edit,
    Delete,
}

/// Renders Edit and Delete for the user's own remote message. Neither
/// carries a provider target: the feature rechecks the selected-history row
/// when the user clicks, and Delete opens a confirmation first.
fn draw_remote_message_actions(
    ui: &mut egui::Ui,
    message: &Message,
    view: RemoteMessageActionsView,
) -> Option<RemoteRowAction> {
    let mut requested = None;
    let mut draw = |ui: &mut egui::Ui| {
        let button = |label: &'static str, enabled: bool| {
            egui::Button::new(
                RichText::new(label)
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(if enabled { theme::fg1() } else { theme::fg3() }),
            )
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .corner_radius(egui::CornerRadius::same(10))
        };
        if view.show_edit {
            let response = ui
                .add_enabled(view.edit_enabled, button("Edit", view.edit_enabled))
                .on_hover_text(if view.edit_enabled {
                    "Edit this message for everyone in the chat"
                } else {
                    "Send or clear your draft before editing a message"
                });
            if response.clicked() {
                requested = Some(RemoteRowAction::Edit);
            }
        }
        if view.show_delete {
            let response = ui
                .add(button("Delete", true))
                .on_hover_text("Delete this message for everyone in the chat");
            if response.clicked() {
                requested = Some(RemoteRowAction::Delete);
            }
        }
    };
    if message.own {
        ui.with_layout(Layout::right_to_left(Align::Min), draw);
    } else {
        ui.horizontal(|ui| {
            ui.add_space(40.0);
            draw(ui);
        });
    }
    requested
}

/// Renders the narrow remote-only entry point for Graph's one-message quote
/// reply. It carries no provider target into the shared message widget: the
/// feature rechecks the selected history row when the user clicks it.
fn draw_remote_quote_reply_control(
    ui: &mut egui::Ui,
    message: &Message,
    quote: super::remote::RemoteChatQuoteReplyView,
) -> bool {
    let mut requested = false;
    let mut draw = |ui: &mut egui::Ui| {
        let response = ui.add_enabled(
            quote.can_quote,
            egui::Button::new(
                RichText::new(if quote.selected {
                    "Remove from quote"
                } else {
                    "Quote"
                })
                .font(theme::regular(theme::font_size::CAPTION))
                .color(if quote.can_quote {
                    theme::fg1()
                } else {
                    theme::fg3()
                }),
            )
            .fill(theme::surface())
            .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
            .corner_radius(egui::CornerRadius::same(10)),
        );
        let label = if quote.selected {
            "Remove this message from your quote reply"
        } else if quote.can_quote {
            "Add this Microsoft 365 message to your quote reply"
        } else if quote.limit_reached {
            "A reply can quote at most 10 messages"
        } else {
            "This Microsoft 365 message is not available to quote"
        };
        if response.on_hover_text(label).clicked() {
            requested = true;
        }
    };
    if message.own {
        ui.with_layout(Layout::right_to_left(Align::Min), draw);
    } else {
        ui.horizontal(|ui| {
            ui.add_space(40.0);
            draw(ui);
        });
    }
    requested
}

/// Centered day divider: a hairline on each side of the day label.
fn draw_day_divider(ui: &mut egui::Ui, label: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        theme::regular(theme::font_size::CAPTION),
        theme::fg3(),
    );
    let gap = 12.0;
    let half = galley.size().x / 2.0 + gap;
    let y = rect.center().y;
    let stroke = Stroke::new(1.0_f32, theme::stroke_subtle());
    let margin = theme::space::XL;
    ui.painter()
        .hline(rect.left() + margin..=rect.center().x - half, y, stroke);
    ui.painter()
        .hline(rect.center().x + half..=rect.right() - margin, y, stroke);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, theme::fg3());
}
