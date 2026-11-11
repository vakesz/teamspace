//! Leaf egui rendering for reusable Chat sidebar rows.
//!
//! The Chat state module owns filtering, selection, and lifecycle actions.
//! This module keeps the repeated visual row mechanics local to the feature,
//! so changes to sidebar presentation do not obscure those domain paths.

use super::{LocalConversation, LocalConversationAction};
use crate::data::workspace::ChatListItem;
use crate::ui::avatar::{Avatar, AvatarSize};
use crate::ui::list_row::ListRow;
use eframe::egui;

pub(super) fn draw_favorite_sidebar_row(
    ui: &mut egui::Ui,
    favorite: &ChatListItem,
    selected: bool,
    unread: bool,
) -> bool {
    draw_chat_list_item_row(ui, favorite, selected, unread)
}

pub(super) fn draw_workspace_sidebar_row(
    ui: &mut egui::Ui,
    row: &ChatListItem,
    selected: bool,
    unread: bool,
) -> bool {
    draw_chat_list_item_row(ui, row, selected, unread)
}

fn draw_chat_list_item_row(
    ui: &mut egui::Ui,
    row: &ChatListItem,
    selected: bool,
    unread: bool,
) -> bool {
    ListRow::new(&row.name)
        .timestamp(&row.time)
        .preview(&row.preview)
        .selected(selected)
        .unread(unread)
        .avatar(Avatar::new(&row.name, AvatarSize::Md32))
        .show(ui)
        .clicked()
}

/// Draws one mutable local-conversation row wherever it is projected in the
/// sidebar. A local pin changes only placement (Favorites vs. Chats), not the
/// durable conversation identity or its selection target.
pub(super) fn draw_local_sidebar_row(
    ui: &mut egui::Ui,
    row: &LocalConversation,
    selected: bool,
    pinned: bool,
) -> (bool, Option<LocalConversationAction>) {
    let mut row_action = None;
    let preview = if pinned {
        "Local preview · pinned"
    } else {
        "Local preview conversation"
    };
    let response = ListRow::new(&row.name)
        .preview(preview)
        .selected(selected)
        .avatar(Avatar::new(&row.name, AvatarSize::Md32))
        .hover_menu(|ui| {
            if ui
                .button(if pinned {
                    "Unpin local chat"
                } else {
                    "Pin local chat"
                })
                .clicked()
            {
                row_action = Some(if pinned {
                    LocalConversationAction::Unpin(row.id)
                } else {
                    LocalConversationAction::Pin(row.id)
                });
                ui.close();
            }
            if ui.button("Rename local chat").clicked() {
                row_action = Some(LocalConversationAction::Rename(row.id));
                ui.close();
            }
            if ui.button("Remove local chat…").clicked() {
                row_action = Some(LocalConversationAction::Remove(row.id));
                ui.close();
            }
        })
        .show(ui);
    (response.clicked() && row_action.is_none(), row_action)
}
