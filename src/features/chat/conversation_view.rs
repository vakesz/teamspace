use super::ChatState;
use super::remote::{RemoteChatMembersPanel, RemotePinnedMessagesPanel};
use crate::features::calls::CallMode;
use crate::shared::model::ChatTab;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::icon::{Icon, icon_button};
use crate::ui::link_preview::{self};
use crate::ui::theme;
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText, Stroke};

use super::{ConversationSelection, LocalConversationAction};

impl ChatState {
    pub(crate) fn draw_conversation(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        let remote_selected = matches!(self.selected, ConversationSelection::Remote(_));
        let remote_index = match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            ConversationSelection::Favorite(_)
            | ConversationSelection::Workspace(_)
            | ConversationSelection::Local(_) => None,
        };
        let remote_members_open =
            remote_index.is_some_and(|index| self.remote_chats.has_members_panel(index));
        let remote_pinned_messages_open =
            remote_index.is_some_and(|index| self.remote_chats.has_pinned_panel(index));
        if remote_selected {
            // Secondary local-preview tabs own mutable notes and attachment
            // flows. Remote text send has its own small reconciliation seam,
            // while every other remote mutation stays unavailable.
            self.tab = ChatTab::Chat;
        }
        let (name, _subtitle, _initials, _avatar_color) = self.selected_details();
        let compact_header = ui.available_width() < 780.0;
        let selected_local_id = self.selected_local_conversation_id();
        let mut local_conversation_action = None;
        let mut toggle_remote_pinned_messages = None;
        let notify = self.notifications_enabled();
        let mut open_notification_settings = false;
        let can_mute = remote_index.is_some_and(|index| self.can_mute_remote_chat(index));
        let mut muted = remote_index.is_some_and(|index| self.is_remote_chat_muted(index));
        let mut toggle_remote_members = None;
        let head = egui::TopBottomPanel::top("conversation_head")
            .exact_height(if compact_header { 92.0 } else { 56.0 })
            .frame(
                egui::Frame::new()
                    .fill(theme::surface())
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    Avatar::new(&name, AvatarSize::Md32).show(ui);
                    ui.label(
                        RichText::new(&name)
                            .font(theme::semibold(theme::font_size::SUBTITLE))
                            .color(theme::fg1()),
                    );
                    if !compact_header && !remote_selected {
                        ui.add_space(theme::space::SM);
                        self.draw_tabs_inline(ui);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let more = icon_button(ui, Icon::More, "More conversation options");
                        egui::Popup::menu(&more).show(|ui| {
                            if remote_selected {
                                ui.label(
                                    RichText::new(
                                        "Microsoft 365 chat · text and file send enabled",
                                    )
                                    .font(theme::regular(theme::font_size::SMALL))
                                    .color(theme::fg3()),
                                );
                                ui.separator();
                                if ui
                                    .button(if remote_pinned_messages_open {
                                        "Close pinned messages"
                                    } else {
                                        "Pinned messages"
                                    })
                                    .clicked()
                                {
                                    toggle_remote_pinned_messages = remote_index;
                                    ui.close();
                                }
                                ui.add_enabled(
                                    notify && can_mute,
                                    egui::Checkbox::new(&mut muted, "Mute this chat"),
                                );
                                if ui.button("All notification settings…").clicked() {
                                    open_notification_settings = true;
                                    ui.close();
                                }
                                if ui
                                    .button(if remote_members_open {
                                        "Close people"
                                    } else {
                                        "People"
                                    })
                                    .clicked()
                                {
                                    toggle_remote_members = remote_index;
                                    ui.close();
                                }
                            } else {
                                if ui.button("View shared items").clicked() {
                                    self.tab = ChatTab::Shared;
                                    ui.close();
                                }
                                if ui.button("Open notes").clicked() {
                                    self.tab = ChatTab::Notes;
                                    ui.close();
                                }
                                if ui.button("Clear message draft").clicked() {
                                    if let Some(composer) = self.selected_composer_mut() {
                                        composer.draft.clear();
                                        composer.pending_attachment = None;
                                        composer.attachment_picker_open = false;
                                        self.mark_persistence_dirty();
                                        toast.show("Message draft cleared");
                                    }
                                    ui.close();
                                }
                                if let Some(local_id) = selected_local_id {
                                    ui.separator();
                                    if ui
                                        .button(if self.is_local_conversation_pinned(local_id) {
                                            "Unpin local chat"
                                        } else {
                                            "Pin local chat"
                                        })
                                        .clicked()
                                    {
                                        local_conversation_action =
                                            Some(if self.is_local_conversation_pinned(local_id) {
                                                LocalConversationAction::Unpin(local_id)
                                            } else {
                                                LocalConversationAction::Pin(local_id)
                                            });
                                        ui.close();
                                    }
                                    if ui.button("Rename local chat").clicked() {
                                        local_conversation_action =
                                            Some(LocalConversationAction::Rename(local_id));
                                        ui.close();
                                    }
                                    if ui.button("Remove local chat…").clicked() {
                                        local_conversation_action =
                                            Some(LocalConversationAction::Remove(local_id));
                                        ui.close();
                                    }
                                }
                            }
                        });
                        if icon_button(ui, Icon::Search, "Search chats").clicked() {
                            self.chat_search_open = true;
                        }
                        if !remote_selected {
                            if icon_button(ui, Icon::PersonAdd, "Add people").clicked() {
                                toast.show("Adding people is not available in this local preview.");
                            }
                            let call_options = icon_button(ui, Icon::ChevronDown, "Call options");
                            egui::Popup::menu(&call_options).show(|ui| {
                                if ui.button("Video call").clicked() {
                                    self.request_call(name.clone(), CallMode::Video);
                                    ui.close();
                                }
                                if ui.button("Audio call").clicked() {
                                    self.request_call(name.clone(), CallMode::Audio);
                                    ui.close();
                                }
                            });
                            if icon_button(ui, Icon::Call, "Start a local audio call").clicked() {
                                self.request_call(name.clone(), CallMode::Audio);
                            }
                        }
                    });
                });
                if compact_header && !remote_selected {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| self.draw_tabs_inline(ui));
                }
            });
        let head_rect = head.response.rect;
        ui.painter().hline(
            head_rect.x_range(),
            head_rect.bottom(),
            Stroke::new(1.0_f32, theme::stroke_subtle()),
        );
        if let Some(action) = local_conversation_action {
            self.handle_local_conversation_action(action, toast);
        }
        if open_notification_settings {
            self.settings_requested = true;
        }
        if remote_selected && let Some(index) = remote_index.filter(|_| can_mute) {
            self.set_remote_chat_muted(index, muted);
        }
        if let Some(index) = toggle_remote_pinned_messages {
            self.toggle_remote_chat_pinned_messages(index);
        }
        if let Some(index) = toggle_remote_members {
            self.toggle_remote_chat_members(index);
        }
        self.draw_remote_pinned_messages_panel(ui.ctx());
        self.draw_remote_members_panel(ui.ctx());

        if remote_selected || self.tab == ChatTab::Chat {
            self.draw_message_tab(ui, toast);
        } else {
            self.draw_secondary_tab(ui, &name, toast);
        }
        self.draw_shared_preview(ui.ctx());
        link_preview::draw(ui.ctx(), &mut self.link_preview, "Shared in this chat");
        self.draw_message_editor(ui.ctx(), toast);
        self.draw_local_conversation_editor(ui.ctx(), toast);
        self.draw_local_conversation_removal(ui.ctx(), toast);
        self.draw_remote_delete_confirmation(ui.ctx(), toast);
    }

    /// Opens or closes only the selected Microsoft 365 chat's contextual
    /// pinned-message panel. Opening produces one opaque read target; closing
    /// first clears safe rows, then asks the app-owned transport controller to
    /// cancel only a matching selected-pin poll.
    pub(super) fn toggle_remote_chat_pinned_messages(&mut self, remote_index: usize) {
        // The More menu supplies the selected index, but enforce that policy
        // at the state transition too so a future caller cannot request pins
        // for an off-screen remote conversation.
        if self.selected != ConversationSelection::Remote(remote_index) {
            return;
        }
        if self.remote_chats.has_pinned_panel(remote_index) {
            if self.remote_chats.close_pinned_messages(remote_index) {
                self.pending_remote_chat_pinned_messages = None;
                self.cancel_remote_chat_pinned_messages_requested = true;
            }
            return;
        }
        if let Some(chat) = self.remote_chats.open_pinned_messages(remote_index) {
            self.pending_remote_chat_pinned_messages = Some(chat);
        }
    }

    /// Opens or closes only the selected Microsoft 365 chat's contextual
    /// People panel. Opening produces one opaque read target; closing clears
    /// rows first, then asks the controller to cancel only the members poll.
    pub(super) fn toggle_remote_chat_members(&mut self, remote_index: usize) {
        if self.selected != ConversationSelection::Remote(remote_index) {
            return;
        }
        if self.remote_chats.has_members_panel(remote_index) {
            if self.remote_chats.close_members(remote_index) {
                self.pending_remote_chat_members = None;
                self.cancel_remote_chat_members_requested = true;
            }
            return;
        }
        if let Some(chat) = self.remote_chats.open_members(remote_index) {
            self.pending_remote_chat_members = Some(chat);
        }
    }

    /// Draws a narrow, read-only roster. The model has already discarded
    /// member IDs, emails, and provider fields; rows offer no action.
    fn draw_remote_members_panel(&mut self, ctx: &egui::Context) {
        let ConversationSelection::Remote(remote_index) = self.selected else {
            return;
        };
        let Some(panel) = self.remote_chats.members_panel(remote_index) else {
            return;
        };

        let mut open = true;
        egui::Window::new("People")
            .id(egui::Id::new("remote_chat_members_panel"))
            .default_width(320.0)
            .default_height(240.0)
            .resizable(false)
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| match panel {
                RemoteChatMembersPanel::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(theme::space::XL));
                        ui.label(
                            RichText::new("Loading people…")
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                }
                RemoteChatMembersPanel::AccessDenied => {
                    ui.label(
                        RichText::new(
                            "Microsoft 365 did not allow member access for this chat. Check your chat access and reconnect to try again.",
                        )
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteChatMembersPanel::Unavailable => {
                    ui.label(
                        RichText::new(
                            "People could not be loaded right now. Close this panel and open it again to retry.",
                        )
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemoteChatMembersPanel::Ready([]) => {
                    ui.label(
                        RichText::new("No members to show for this chat.")
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
                RemoteChatMembersPanel::Ready(members) => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for member in members {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&member.display_name)
                                            .font(theme::semibold(theme::font_size::BODY))
                                            .color(theme::fg1()),
                                    );
                                    if let Some(presence) = member.presence {
                                        draw_presence_dot(ui, presence);
                                        ui.label(
                                            RichText::new(presence.label())
                                                .font(theme::regular(theme::font_size::CAPTION))
                                                .color(theme::fg3()),
                                        );
                                    }
                                    if let Some(role) = &member.role_label {
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(role)
                                                        .font(theme::regular(
                                                            theme::font_size::CAPTION,
                                                        ))
                                                        .color(theme::fg3()),
                                                );
                                            },
                                        );
                                    }
                                });
                                ui.add_space(theme::space::SM);
                            }
                        });
                }
            });
        if !open && self.remote_chats.close_members(remote_index) {
            self.pending_remote_chat_members = None;
            self.cancel_remote_chat_members_requested = true;
        }
    }

    /// Draws a narrow, presentation-only panel over the chat. The model has
    /// already discarded provider routes, pin IDs, attachments, reactions,
    /// and HTML; this UI deliberately offers no row action or mutation path.
    fn draw_remote_pinned_messages_panel(&mut self, ctx: &egui::Context) {
        let Some(remote_index) = (match self.selected {
            ConversationSelection::Remote(index) => Some(index),
            ConversationSelection::Favorite(_)
            | ConversationSelection::Workspace(_)
            | ConversationSelection::Local(_) => None,
        }) else {
            return;
        };
        let Some(panel) = self.remote_chats.pinned_messages_panel(remote_index) else {
            return;
        };

        let mut open = true;
        egui::Window::new("Pinned messages")
            .id(egui::Id::new("remote_chat_pinned_messages_panel"))
            .default_width(360.0)
            .default_height(260.0)
            .resizable(false)
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| match panel {
                RemotePinnedMessagesPanel::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(theme::space::XL));
                        ui.label(
                            RichText::new("Loading pinned messages…")
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::fg3()),
                        );
                    });
                }
                RemotePinnedMessagesPanel::AccessDenied => {
                    ui.label(
                        RichText::new(
                            "Microsoft 365 did not allow pinned-message access for this chat. Check your chat access and reconnect to try again.",
                        )
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemotePinnedMessagesPanel::Unavailable => {
                    ui.label(
                        RichText::new(
                            "Pinned messages could not be loaded right now. Close this panel and open it again to retry.",
                        )
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                    );
                }
                RemotePinnedMessagesPanel::Ready([]) => {
                    ui.label(
                        RichText::new("No pinned messages in this chat.")
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                }
                RemotePinnedMessagesPanel::Ready(messages) => {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for message in messages {
                                egui::Frame::new()
                                    .fill(theme::canvas())
                                    .stroke(Stroke::new(1.0_f32, theme::stroke_subtle()))
                                    .corner_radius(egui::CornerRadius::same(theme::radius::SM))
                                    .inner_margin(egui::Margin::symmetric(10, 8))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(&message.sender_name)
                                                    .font(theme::semibold(theme::font_size::BODY))
                                                    .color(theme::fg1()),
                                            );
                                            if !message.sent_at.is_empty() {
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        ui.label(
                                                            RichText::new(&message.sent_at)
                                                                .font(theme::regular(
                                                                    theme::font_size::CAPTION,
                                                                ))
                                                                .color(theme::fg3()),
                                                        );
                                                    },
                                                );
                                            }
                                        });
                                        ui.add_space(2.0);
                                        ui.label(
                                            RichText::new(&message.plain_text)
                                                .font(theme::regular(theme::font_size::BODY))
                                                .color(theme::fg1()),
                                        );
                                        if message.deleted {
                                            ui.add_space(2.0);
                                            ui.label(
                                                RichText::new("Deleted in Microsoft 365")
                                                    .font(theme::regular(
                                                        theme::font_size::CAPTION,
                                                    ))
                                                    .color(theme::fg3()),
                                            );
                                        }
                                    });
                                ui.add_space(theme::space::SM);
                            }
                        });
                }
            });
        if !open && self.remote_chats.close_pinned_messages(remote_index) {
            self.pending_remote_chat_pinned_messages = None;
            self.cancel_remote_chat_pinned_messages_requested = true;
        }
    }
}

/// A small filled circle in a presence theme token.
fn draw_presence_dot(ui: &mut egui::Ui, presence: crate::integrations::RemoteMemberPresence) {
    use crate::integrations::RemoteMemberPresence as P;
    let color = match presence {
        P::Available => theme::presence_available(),
        P::Busy => theme::presence_busy(),
        P::DoNotDisturb => theme::presence_dnd(),
        P::Away | P::BeRightBack => theme::presence_away(),
        P::Offline => theme::presence_offline(),
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}
