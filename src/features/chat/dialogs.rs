use super::ChatState;
use super::persistence::{
    MAX_LOCAL_CONVERSATION_NAME_BYTES, MAX_MESSAGE_TEXT_BYTES, StoredConversation,
    StoredLocalConversation,
};
use crate::ui::modal;
use crate::ui::shortcuts::{multiline_submit_pressed, singleline_submit_pressed, take_escape};
use crate::ui::theme;
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText};

use super::{
    ComposerState, ConversationSelection, LocalConversation, LocalConversationMutationError,
    MessageTimeline, local_conversation_initials, message_to_stored, outgoing_message, rgb,
};

impl ChatState {
    pub(super) fn draw_message_editor(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        let Some(mut editor) = self.editing_message.take() else {
            return;
        };
        if take_escape(ctx) || self.selected_chat_id().as_deref() != Some(editor.chat_id.as_str()) {
            return;
        }

        let mut save = false;
        let mut cancel = false;
        let focus_requested = editor.focus_requested;
        editor.focus_requested = false;
        let _ = modal::show(ctx, "edit_local_message", 460.0, |ui| {
            modal::heading(ui, "Edit message");
            ui.add_space(6.0);
            ui.label(
                RichText::new("Only locally authored messages can be changed here.")
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
            ui.add_space(10.0);
            let input_id = ui.make_persistent_id(("edit_local_message", editor.local_message_id));
            let input = ui.add_sized(
                [ui.available_width(), 90.0],
                egui::TextEdit::multiline(&mut editor.draft)
                    .id(input_id)
                    .hint_text("Write a message"),
            );
            if focus_requested {
                input.request_focus();
            }
            if input.has_focus() && multiline_submit_pressed(ui) {
                save = true;
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Save").color(theme::fg_on_brand()))
                            .fill(theme::brand())
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            ui.label(
                RichText::new(format!(
                    "Press {} to save",
                    crate::ui::shortcuts::submit_chord()
                ))
                .font(theme::regular(theme::font_size::MICRO))
                .color(theme::fg3()),
            );
        });

        if cancel {
            return;
        }
        if save {
            match self.edit_local_message(editor.local_message_id, &editor.draft) {
                Ok(()) => toast.show("Local message updated"),
                Err(error) => {
                    toast.show_error(error.user_message());
                    self.editing_message = Some(editor);
                }
            }
        } else {
            self.editing_message = Some(editor);
        }
    }

    pub(super) fn draw_local_conversation_editor(
        &mut self,
        ctx: &egui::Context,
        toast: &mut ToastState,
    ) {
        let Some(mut editor) = self.editing_local_conversation.take() else {
            return;
        };
        if take_escape(ctx) {
            return;
        }
        if self.local_index_for_id(editor.id).is_none() {
            toast.show_error(LocalConversationMutationError::Missing.user_message());
            return;
        }

        let mut save = false;
        let mut cancel = false;
        let focus_requested = editor.focus_requested;
        editor.focus_requested = false;
        let _ = modal::show(ctx, "rename_local_chat", 420.0, |ui| {
            modal::heading(ui, "Rename local chat");
            ui.add_space(6.0);
            ui.label(
                RichText::new("This changes only the local preview conversation.")
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new("Name")
                    .font(theme::semibold(theme::font_size::CAPTION))
                    .color(theme::fg1()),
            );
            let input_id = ui.make_persistent_id(("rename_local_chat", editor.id));
            let input = ui.add_sized(
                [ui.available_width(), 34.0],
                egui::TextEdit::singleline(&mut editor.draft)
                    .id(input_id)
                    .hint_text("Name or group"),
            );
            if focus_requested {
                input.request_focus();
            }
            if singleline_submit_pressed(&input, ui) {
                save = true;
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Save").color(theme::fg_on_brand()))
                            .fill(theme::brand())
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            ui.label(
                RichText::new("Press Enter to save")
                    .font(theme::regular(theme::font_size::MICRO))
                    .color(theme::fg3()),
            );
        });

        if cancel {
            return;
        }
        if save {
            match self.rename_local_conversation(editor.id, &editor.draft) {
                Ok(()) => toast.show("Local chat renamed"),
                Err(error) => {
                    toast.show_error(error.user_message());
                    self.editing_local_conversation = Some(editor);
                }
            }
        } else {
            self.editing_local_conversation = Some(editor);
        }
    }

    /// The explicit confirmation for deleting an own Microsoft 365 message.
    /// It is dropped if the chat is no longer the selected remote chat.
    pub(super) fn draw_remote_delete_confirmation(
        &mut self,
        ctx: &egui::Context,
        toast: &mut ToastState,
    ) {
        let Some(confirmation) = self.confirming_remote_delete.take() else {
            return;
        };
        let still_selected = match self.selected {
            ConversationSelection::Remote(index) => {
                self.remote_chats.index_of(&confirmation.chat) == Some(index)
            }
            _ => false,
        };
        if take_escape(ctx) || !still_selected {
            return;
        }

        let mut delete = false;
        let mut cancel = false;
        let _ = modal::show(ctx, "delete_remote_message", 420.0, |ui| {
            modal::heading(ui, "Delete this message?");
            ui.add_space(6.0);
            ui.label(
                RichText::new("Delete this message for everyone in the chat?")
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg1()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(&confirmation.preview)
                    .font(theme::regular(theme::font_size::MICRO))
                    .color(theme::fg3()),
            );
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Delete").color(theme::fg_on_brand()))
                            .fill(theme::danger())
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    delete = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

        if cancel {
            return;
        }
        if delete {
            self.confirming_remote_delete = Some(confirmation);
            if let Err(error) = self.confirm_remote_delete() {
                toast.show_error(error.user_message());
            }
        } else {
            self.confirming_remote_delete = Some(confirmation);
        }
    }

    pub(super) fn draw_local_conversation_removal(
        &mut self,
        ctx: &egui::Context,
        toast: &mut ToastState,
    ) {
        let Some(removal) = self.removing_local_conversation.take() else {
            return;
        };
        if take_escape(ctx) {
            return;
        }
        if self.local_index_for_id(removal.id).is_none() {
            toast.show_error(LocalConversationMutationError::Missing.user_message());
            return;
        }

        let mut remove = false;
        let mut cancel = false;
        let _ = modal::show(ctx, "remove_local_chat", 420.0, |ui| {
            modal::heading(ui, "Remove local chat?");
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Remove \"{}\" and its local messages, notes, and drafts?",
                    removal.name
                ))
                .font(theme::regular(theme::font_size::SMALL))
                .color(theme::fg1()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("Seeded and reference chats cannot be removed here.")
                    .font(theme::regular(theme::font_size::MICRO))
                    .color(theme::fg3()),
            );
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Remove").color(theme::fg_on_brand()))
                            .fill(theme::danger())
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    remove = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });

        if cancel {
            return;
        }
        if remove {
            match self.remove_local_conversation(removal.id) {
                Ok(()) => toast.show("Local chat removed"),
                Err(error) => {
                    toast.show_error(error.user_message());
                    self.removing_local_conversation = Some(removal);
                }
            }
        } else {
            self.removing_local_conversation = Some(removal);
        }
    }

    pub(super) fn draw_new_chat_dialog(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        if !self.new_chat_open {
            return;
        }
        if take_escape(ctx) {
            self.new_chat_open = false;
            return;
        }

        let mut open = self.new_chat_open;
        let mut create = false;
        let mut cancel = false;
        let focus_recipient = self.new_chat_focus_requested;
        self.new_chat_focus_requested = false;
        let _ = modal::show(ctx, "new_chat_dialog", 420.0, |ui| {
            modal::heading(ui, "New chat");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Start a local preview conversation")
                    .strong()
                    .color(theme::fg1()),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new("To")
                    .font(theme::semibold(theme::font_size::CAPTION))
                    .color(theme::fg1()),
            );
            let recipient_id = ui.make_persistent_id("new_chat_recipient");
            let recipient_input = ui.add_sized(
                [ui.available_width(), 34.0],
                egui::TextEdit::singleline(&mut self.new_chat_recipient)
                    .id(recipient_id)
                    .hint_text("Name or group"),
            );
            if focus_recipient {
                recipient_input.request_focus();
            }
            ui.add_space(10.0);
            ui.label(
                RichText::new("Message (optional)")
                    .font(theme::semibold(theme::font_size::CAPTION))
                    .color(theme::fg1()),
            );
            let message_input = ui.add_sized(
                [ui.available_width(), 78.0],
                egui::TextEdit::multiline(&mut self.new_chat_message)
                    .hint_text("Write your first message"),
            );
            if (recipient_input.has_focus() || message_input.has_focus())
                && multiline_submit_pressed(ui)
            {
                create = true;
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Start chat").color(theme::fg_on_brand()))
                            .fill(theme::brand())
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    create = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            ui.label(
                RichText::new(format!(
                    "Press {} to start a chat while writing a first message",
                    crate::ui::shortcuts::submit_chord()
                ))
                .font(theme::regular(theme::font_size::MICRO))
                .color(theme::fg3()),
            );
        });

        if cancel {
            open = false;
        }
        if create {
            if self.start_new_chat().is_ok() {
                open = false;
                toast.show("Local preview conversation created");
            } else if self.new_chat_recipient.trim().is_empty() {
                toast.show_error("Enter a name or group before starting a chat.");
            } else {
                toast.show_error("That local chat cannot be saved in this profile.");
            }
        }
        self.new_chat_open = open;
    }

    pub(super) fn start_new_chat(&mut self) -> Result<(), ()> {
        let recipient = self.new_chat_recipient.trim().to_owned();
        if recipient.is_empty() {
            return Err(());
        }
        let first_message = self.new_chat_message.trim().to_owned();
        self.create_local_conversation(recipient, first_message)
            .then_some(())
            .ok_or(())
    }

    pub(super) fn create_local_conversation(
        &mut self,
        name: String,
        first_message: String,
    ) -> bool {
        let name = name.trim().to_owned();
        let first_message = first_message.trim().to_owned();
        if name.is_empty()
            || name.len() > MAX_LOCAL_CONVERSATION_NAME_BYTES
            || first_message.len() > MAX_MESSAGE_TEXT_BYTES
        {
            return false;
        }
        let id = self.next_local_conversation_id;
        let Some(next_local_conversation_id) = id.checked_add(1) else {
            return false;
        };
        let initials = local_conversation_initials(&name);
        let color = theme::local_conversation_accent();
        let initial_message =
            (!first_message.is_empty()).then(|| outgoing_message(first_message.clone(), None));
        let (initial_message_id, next_local_message_id) = if initial_message.is_some() {
            let Some((local_message_id, next_local_message_id)) = self.next_local_message_id_pair()
            else {
                return false;
            };
            (Some(local_message_id), next_local_message_id)
        } else {
            (None, self.next_local_message_id)
        };

        let mut prospective = self.stored();
        prospective.next_local_conversation_id = next_local_conversation_id;
        prospective.next_local_message_id = next_local_message_id;
        prospective
            .local_conversations
            .push(StoredLocalConversation {
                id,
                name: name.clone(),
                initials: initials.clone(),
                color_rgb: rgb(color),
                state: StoredConversation {
                    messages: initial_message
                        .as_ref()
                        .zip(initial_message_id)
                        .map_or_else(Vec::new, |(message, local_message_id)| {
                            vec![message_to_stored(message, Some(local_message_id))]
                        }),
                    ..StoredConversation::default()
                },
            });
        if prospective.validate_for_persistence().is_err() {
            return false;
        }

        let mut messages = MessageTimeline::default();
        let action_body = initial_message.as_ref().map(|message| message.text.clone());
        if let Some((local_message_id, message)) = initial_message_id.zip(initial_message) {
            messages.push_locally_authored(local_message_id, message);
        }
        self.next_local_conversation_id = next_local_conversation_id;
        self.next_local_message_id = next_local_message_id;
        self.local_conversations.push(LocalConversation {
            id,
            name,
            initials,
            color,
            messages,
            notes: String::new(),
            composer: ComposerState::default(),
        });
        self.select_conversation(ConversationSelection::Local(
            self.local_conversations.len() - 1,
        ));
        if let Some(body) = action_body {
            self.enqueue_message_action(body);
        }
        self.mark_persistence_dirty();
        true
    }
}
