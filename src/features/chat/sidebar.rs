use super::ChatState;
use super::remote::draw_remote_sidebar_row;
use super::render::{
    draw_favorite_sidebar_row, draw_local_sidebar_row, draw_workspace_sidebar_row,
};
use crate::data::workspace::workspace;
use crate::ui::icon::{Icon, IconSize, icon_button, icon_button_selected};
use crate::ui::list_row::section_header;
use crate::ui::theme;
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Color32, Layout, RichText};

use super::{ChatQuickView, ConversationSelection, chat_item_matches, quick_view_row};

impl ChatState {
    pub(crate) fn draw_sidebar(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.add_space(theme::space::LG);
        ui.horizontal(|ui| {
            ui.add_space(theme::space::XL);
            ui.label(
                RichText::new("Chat")
                    .font(theme::semibold(theme::font_size::TITLE))
                    .color(theme::fg1()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(theme::space::MD);
                let compose_options = icon_button(ui, Icon::ChevronDown, "New chat options");
                egui::Popup::menu(&compose_options).show(|ui| {
                    if ui.button("New chat").clicked() {
                        self.open_new_chat();
                        ui.close();
                    }
                });
                if icon_button(ui, Icon::Compose, "New chat").clicked() {
                    self.open_new_chat();
                }
                if icon_button_selected(ui, Icon::Filter, self.chat_search_open, "Search chats")
                    .clicked()
                {
                    self.chat_search_open = !self.chat_search_open;
                    if !self.chat_search_open {
                        self.query.clear();
                    }
                }
                let more = icon_button(ui, Icon::More, "More chat options");
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button("Mark all as read").clicked() {
                        let marked = self.mark_all_conversations_read();
                        toast.show(if marked == 0 {
                            "All chats are already read".into()
                        } else {
                            format!(
                                "Marked {marked} chat{} as read",
                                if marked == 1 { "" } else { "s" }
                            )
                        });
                        ui.close();
                    }
                    if ui.button("Clear chat search").clicked() {
                        self.query.clear();
                        self.chat_search_open = false;
                        toast.show("Chat search cleared");
                        ui.close();
                    }
                });
            });
        });
        if self.chat_search_open {
            ui.add_space(theme::space::MD);
            ui.horizontal(|ui| {
                ui.add_space(theme::space::XL);
                ui.add_sized(
                    [ui.available_width() - 2.0 * theme::space::XL, 32.0],
                    egui::TextEdit::singleline(&mut self.query).hint_text("Search chats"),
                );
            });
        }
        ui.add_space(theme::space::SM);
        egui::TopBottomPanel::bottom("chat_sidebar_footer")
            .exact_height(44.0)
            .frame(egui::Frame::new().fill(Color32::TRANSPARENT))
            .show_inside(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(theme::space::XL);
                    ui.add(Icon::Add.image(IconSize::S16).tint(theme::brand_fg()));
                    if ui
                        .link(
                            RichText::new("Add a team or chat")
                                .font(theme::regular(theme::font_size::CAPTION))
                                .color(theme::brand_fg()),
                        )
                        .clicked()
                    {
                        self.open_new_chat();
                    }
                });
            });
        let mention_needle = self.mention_needle();
        egui::ScrollArea::vertical()
            .id_salt("chat_sidebar_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(8, 0))
                    .show(ui, |ui| {
                        self.draw_sidebar_sections(ui, toast, &mention_needle);
                    });
            });
        self.draw_new_chat_dialog(ui.ctx(), toast);
    }

    fn draw_sidebar_sections(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        mention_needle: &str,
    ) {
        let search_term = self.query.trim().to_lowercase();

        section_header(ui, "Quick views", &mut self.sidebar_sections_open[0]);
        if self.sidebar_sections_open[0] {
            for (view, icon, label) in [
                (ChatQuickView::Mentions, Icon::Mention, "Mentions"),
                (ChatQuickView::Drafts, Icon::Drafts, "Drafts"),
            ] {
                let selected = self.quick_view == view;
                if quick_view_row(ui, icon, label, selected).clicked() {
                    self.quick_view = if selected { ChatQuickView::All } else { view };
                }
            }
        }

        let mut selected_local_id = None;
        let mut selected_remote_index = None;
        let mut local_conversation_action = None;
        let mut has_visible_chat = false;

        section_header(ui, "Favorites", &mut self.sidebar_sections_open[1]);
        let favorites_open = self.sidebar_sections_open[1];
        for (idx, favorite) in workspace().chat.favorites.iter().enumerate() {
            let selection = ConversationSelection::Favorite(idx);
            if !self.matches_quick_view_with(selection, mention_needle)
                || !chat_item_matches(favorite, &search_term)
            {
                continue;
            }
            has_visible_chat = true;
            if favorites_open
                && draw_favorite_sidebar_row(
                    ui,
                    favorite,
                    self.selected == selection,
                    self.is_unread(selection),
                )
            {
                self.select_favorite_conversation(idx);
            }
        }
        for (idx, local) in self.local_conversations.iter().enumerate() {
            let selection = ConversationSelection::Local(idx);
            if !self.is_local_conversation_pinned(local.id)
                || !self.matches_quick_view_with(selection, mention_needle)
                || (!search_term.is_empty() && !local.name.to_lowercase().contains(&search_term))
            {
                continue;
            }
            has_visible_chat = true;
            if !favorites_open {
                continue;
            }
            let (clicked, row_action) =
                draw_local_sidebar_row(ui, local, self.selected == selection, true);
            if clicked {
                selected_local_id = Some(local.id);
            }
            if local_conversation_action.is_none() {
                local_conversation_action = row_action;
            }
        }

        section_header(ui, "Chats", &mut self.sidebar_sections_open[2]);
        let chats_open = self.sidebar_sections_open[2];
        if chats_open && self.quick_view != ChatQuickView::All {
            ui.horizontal(|ui| {
                ui.add_space(theme::space::MD);
                ui.label(
                    RichText::new(format!("Showing {}", self.quick_view.label()))
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                );
                if ui.small_button("Clear").clicked() {
                    self.quick_view = ChatQuickView::All;
                }
            });
        }
        let rows = &workspace().chat.conversations;
        for (idx, row) in rows.iter().enumerate() {
            let selection = ConversationSelection::Workspace(idx);
            if !self.matches_quick_view_with(selection, mention_needle)
                || !chat_item_matches(row, &search_term)
            {
                continue;
            }
            has_visible_chat = true;
            if chats_open
                && draw_workspace_sidebar_row(
                    ui,
                    row,
                    self.selected == selection,
                    self.is_unread(selection),
                )
            {
                self.select_workspace_conversation(idx);
            }
        }
        for (idx, row) in self.local_conversations.iter().enumerate() {
            let selection = ConversationSelection::Local(idx);
            if self.is_local_conversation_pinned(row.id)
                || !self.matches_quick_view_with(selection, mention_needle)
                || (!search_term.is_empty() && !row.name.to_lowercase().contains(&search_term))
            {
                continue;
            }
            has_visible_chat = true;
            if !chats_open {
                continue;
            }
            let (clicked, row_action) =
                draw_local_sidebar_row(ui, row, self.selected == selection, false);
            if clicked {
                selected_local_id = Some(row.id);
            }
            if local_conversation_action.is_none() {
                local_conversation_action = row_action;
            }
        }
        let mut remote_caption_drawn = false;
        for (index, row) in self.remote_chats.iter().enumerate() {
            let selection = ConversationSelection::Remote(index);
            if !self.matches_quick_view_with(selection, mention_needle)
                || (!search_term.is_empty() && !row.matches_query(&search_term))
            {
                continue;
            }
            has_visible_chat = true;
            if !chats_open {
                continue;
            }
            if !remote_caption_drawn {
                remote_caption_drawn = true;
                ui.horizontal(|ui| {
                    ui.add_space(theme::space::MD);
                    ui.label(
                        RichText::new("Microsoft 365 · synced chats, text messages enabled")
                            .font(theme::regular(theme::font_size::CAPTION))
                            .color(theme::fg3()),
                    );
                });
            }
            if draw_remote_sidebar_row(ui, row, self.selected == selection) {
                selected_remote_index = Some(index);
            }
        }
        if let Some(local_id) = selected_local_id {
            self.select_local_conversation_by_id(local_id);
        }
        if let Some(index) = selected_remote_index {
            self.select_remote_conversation(index);
        }
        if let Some(action) = local_conversation_action {
            self.handle_local_conversation_action(action, toast);
        }
        if !has_visible_chat {
            ui.horizontal(|ui| {
                ui.add_space(theme::space::LG);
                ui.label(
                    RichText::new(self.quick_view.empty_message())
                        .font(theme::regular(theme::font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
        }
    }
}
