use super::super::persistence::MAX_LOCAL_CONVERSATION_NAME_BYTES;
use super::super::{
    CallMode, ChatQuickView, ChatState, ConversationSelection, LinkPreview,
    LocalConversationMutationError, MessageAction, seeded_messages,
};
use crate::data::workspace::workspace;
use crate::features::chat::tabs::{recap_for, shared_items};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMessage, RemoteChatMessagesProjection,
    RemoteChatProjection,
};
use crate::shared::model::{ChatTab, Message, MessageAttachment};
use crate::testing::{render_headlessly, with_escape_press};
use crate::ui::theme;
use crate::ui::toast::ToastState;
use eframe::egui;

#[test]
fn renaming_a_local_chat_preserves_its_id_and_recomputes_initials() {
    let mut chat = ChatState::default();
    assert!(
        chat.create_local_conversation("Design review".into(), "Keep this first message".into())
    );
    let local_id = chat.local_conversations[0].id;
    chat.local_conversations[0].notes = "Keep these notes too".into();
    assert!(!chat.take_actions().is_empty());

    assert_eq!(
        chat.rename_local_conversation(local_id, "  Product council  "),
        Ok(())
    );

    let local = &chat.local_conversations[0];
    assert_eq!(local.id, local_id);
    assert_eq!(local.name, "Product council");
    assert_eq!(local.initials, "PC");
    assert_eq!(local.notes, "Keep these notes too");
    assert_eq!(local.messages.messages[0].text, "Keep this first message");
    assert_eq!(chat.selected_chat_id().as_deref(), Some("local:1"));
    assert!(chat.take_actions().is_empty());

    let restored = ChatState::from_stored(chat.stored());
    let restored_local = &restored.local_conversations[0];
    assert_eq!(restored_local.id, local_id);
    assert_eq!(restored_local.name, "Product council");
    assert_eq!(restored_local.initials, "PC");
    assert_eq!(
        restored_local.messages.messages[0].text,
        "Keep this first message"
    );
}

#[test]
fn local_chat_pins_are_durable_stable_and_never_emit_a_transport_action() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Design review".into(), String::new()));
    let first_id = chat.local_conversations[0].id;
    assert!(chat.create_local_conversation("Product council".into(), String::new()));
    let second_id = chat.local_conversations[1].id;
    let second_target = chat
        .search_results("product council")
        .into_iter()
        .find(|result| result.label == "Product council")
        .expect("local chat should remain searchable")
        .target;
    assert!(chat.take_actions().is_empty());
    chat.mark_persisted();

    assert_eq!(chat.set_local_conversation_pinned(first_id, true), Ok(true));
    assert_eq!(chat.pinned_local_conversation_ids, [first_id]);
    assert!(chat.is_persistence_dirty());
    assert_eq!(
        chat.set_local_conversation_pinned(first_id, true),
        Ok(false)
    );

    // Pinning changes sidebar placement only: the durable local vector and
    // stable search target keep selecting the intended second conversation.
    assert_eq!(chat.selected_local_conversation_id(), Some(second_id));
    chat.select_workspace_conversation(0);
    chat.select_search_result(second_target);
    assert_eq!(chat.selected_local_conversation_id(), Some(second_id));
    assert_eq!(
        chat.set_local_conversation_pinned(second_id, true),
        Ok(true)
    );
    assert_eq!(chat.pinned_local_conversation_ids, [first_id, second_id]);
    assert_eq!(
        chat.set_local_conversation_pinned(first_id, false),
        Ok(true)
    );
    assert_eq!(chat.pinned_local_conversation_ids, [second_id]);
    assert!(chat.take_actions().is_empty());

    let restored = ChatState::from_stored(chat.stored());
    assert_eq!(restored.pinned_local_conversation_ids, [second_id]);
    assert_eq!(restored.local_conversations[0].id, first_id);
    assert_eq!(restored.local_conversations[1].id, second_id);
    assert!(restored.is_local_conversation_pinned(second_id));
    assert!(!restored.is_local_conversation_pinned(first_id));
}

#[test]
fn local_chat_pin_lifecycle_rejects_missing_or_unpersistable_changes_atomically() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Design review".into(), String::new()));
    let local_id = chat.local_conversations[0].id;
    chat.mark_persisted();
    let before = chat.stored();

    assert_eq!(
        chat.set_local_conversation_pinned(999, true),
        Err(LocalConversationMutationError::Missing)
    );
    assert_eq!(chat.stored(), before);
    assert!(!chat.is_persistence_dirty());

    // The prospective snapshot validates the same allocator invariant used
    // by atomic persistence before changing the visible pinned collection.
    chat.next_local_conversation_id = 0;
    assert_eq!(
        chat.set_local_conversation_pinned(local_id, true),
        Err(LocalConversationMutationError::NotPersistable)
    );
    assert!(chat.pinned_local_conversation_ids.is_empty());
    assert!(!chat.is_persistence_dirty());
    assert!(chat.take_actions().is_empty());
}

#[test]
fn local_chat_removal_uses_stable_targeting_and_clears_related_state() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("First local chat".into(), String::new()));
    let first_id = chat.local_conversations[0].id;
    assert!(chat.create_local_conversation(
        "Second local chat".into(),
        "Message that belongs to the second chat".into()
    ));
    let second_id = chat.local_conversations[1].id;
    let second_message_id = chat.local_conversations[1]
        .messages
        .local_id_at(0)
        .expect("initial local message has an ID");
    let target = chat
        .search_results("second local")
        .into_iter()
        .find(|result| result.label == "Second local chat")
        .expect("second local chat is searchable")
        .target;
    assert_eq!(target, super::super::ChatSearchTarget::Local(second_id));
    assert!(!chat.take_actions().is_empty());
    assert_eq!(
        chat.set_local_conversation_pinned(second_id, true),
        Ok(true)
    );
    assert_eq!(chat.pinned_local_conversation_ids, [second_id]);

    // Removing the first row shifts the second chat from index 1 to 0.
    // The previously captured target must still resolve to the second ID.
    assert_eq!(chat.remove_local_conversation(first_id), Ok(()));
    assert_eq!(chat.selected, ConversationSelection::Local(0));
    assert_eq!(chat.local_conversations[0].id, second_id);
    chat.select_workspace_conversation(0);
    chat.select_search_result(target);
    assert_eq!(chat.selected, ConversationSelection::Local(0));
    assert_eq!(chat.selected_local_conversation_id(), Some(second_id));
    assert_eq!(chat.pinned_local_conversation_ids, [second_id]);

    chat.open_local_message_editor(second_message_id);
    chat.shared_preview = Some(MessageAttachment::Spreadsheet);
    chat.link_preview = Some(LinkPreview::resource("Second chat link"));
    chat.requested_call = Some(("Second local chat".into(), CallMode::Audio));
    chat.query = "second local".into();
    chat.chat_search_open = true;
    chat.quick_view = ChatQuickView::Drafts;
    assert!(chat.editing_message.is_some());

    assert_eq!(chat.remove_local_conversation(second_id), Ok(()));
    assert!(chat.local_conversations.is_empty());
    assert!(chat.pinned_local_conversation_ids.is_empty());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert!(chat.editing_message.is_none());
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());
    assert!(chat.requested_call.is_none());
    assert!(chat.query.is_empty());
    assert!(!chat.chat_search_open);
    assert_eq!(chat.quick_view, ChatQuickView::All);
    assert!(chat.take_actions().is_empty());
    assert!(
        ChatState::from_stored(chat.stored())
            .local_conversations
            .is_empty()
    );
}

#[test]
fn seeded_conversations_are_not_mutable_through_the_local_lifecycle() {
    let mut chat = ChatState::default();
    let before = chat.stored();

    assert_eq!(
        chat.rename_local_conversation(1, "Attempted seeded rename"),
        Err(LocalConversationMutationError::Missing)
    );
    assert_eq!(
        chat.remove_local_conversation(1),
        Err(LocalConversationMutationError::Missing)
    );

    assert_eq!(chat.stored(), before);
    assert!(chat.take_actions().is_empty());
}

#[test]
fn local_chat_lifecycle_rejects_invalid_or_unpersistable_changes_atomically() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Design review".into(), String::new()));
    let local_id = chat.local_conversations[0].id;
    let original_name = chat.local_conversations[0].name.clone();

    assert_eq!(
        chat.rename_local_conversation(local_id, "   "),
        Err(LocalConversationMutationError::EmptyName)
    );
    let too_long = "x".repeat(MAX_LOCAL_CONVERSATION_NAME_BYTES + 1);
    assert_eq!(
        chat.rename_local_conversation(local_id, &too_long),
        Err(LocalConversationMutationError::NameTooLong)
    );
    assert_eq!(chat.local_conversations[0].name, original_name);

    // The preflight uses the exact encoded snapshot validation. A broken
    // allocator makes both rename and removal reject before touching UI
    // state, even though a removal would otherwise shrink the snapshot.
    chat.next_local_conversation_id = 0;
    assert_eq!(
        chat.rename_local_conversation(local_id, "Would not persist"),
        Err(LocalConversationMutationError::NotPersistable)
    );
    assert_eq!(
        chat.remove_local_conversation(local_id),
        Err(LocalConversationMutationError::NotPersistable)
    );
    assert_eq!(chat.local_conversations.len(), 1);
    assert_eq!(chat.local_conversations[0].id, local_id);
    assert_eq!(chat.local_conversations[0].name, original_name);
}

#[test]
fn search_results_include_local_conversations_and_reselect_them() {
    let mut chat = ChatState::default();
    chat.create_local_conversation("Design review".into(), String::new());
    let local_conversation_id = chat.local_conversations[0].id;

    let result = chat
        .search_results("design")
        .into_iter()
        .find(|result| result.label == "Design review")
        .expect("local chat should be searchable");
    assert_eq!(
        result.target,
        super::super::ChatSearchTarget::Local(local_conversation_id)
    );

    chat.select_workspace_conversation(0);
    chat.select_search_result(result.target);
    assert_eq!(chat.selected, ConversationSelection::Local(0));
}

#[test]
fn search_results_include_seeded_and_local_message_content() {
    let mut chat = ChatState::default();
    let seeded = chat
        .search_results("stain")
        .into_iter()
        .find(|result| result.target == super::super::ChatSearchTarget::Workspace(0))
        .expect("seeded message should be searchable");
    assert!(seeded.detail.contains("stain"));

    chat.create_local_conversation("Project sync".into(), "Review the design brief".into());
    let local_conversation_id = chat.local_conversations[0].id;
    let local = chat
        .search_results("design brief")
        .into_iter()
        .find(|result| {
            result.target == super::super::ChatSearchTarget::Local(local_conversation_id)
        })
        .expect("local message should be searchable");
    assert!(local.detail.contains("Review the design brief"));

    chat.select_search_result(local.target);
    assert_eq!(chat.selected, ConversationSelection::Local(0));
}

#[test]
fn drafts_and_attachments_are_scoped_to_the_selected_conversation() {
    let mut chat = ChatState::default();
    let primary = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    primary.draft = "Primary draft".into();
    primary.pending_attachment = Some(MessageAttachment::LayoutMockup);

    chat.selected = ConversationSelection::Workspace(1);
    assert!(
        chat.selected_composer_mut()
            .expect("test selection has a composer")
            .draft
            .is_empty()
    );
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Second conversation draft".into();

    chat.selected = ConversationSelection::Workspace(0);
    let primary = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    assert_eq!(primary.draft, "Primary draft");
    assert_eq!(
        primary.pending_attachment,
        Some(MessageAttachment::LayoutMockup)
    );
}

#[test]
fn quick_views_filter_drafts_and_mentions_from_real_conversation_state() {
    let mut chat = ChatState::default();
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Follow up".into();
    chat.quick_view = ChatQuickView::Drafts;
    assert!(chat.matches_quick_view(ConversationSelection::Workspace(0)));
    assert!(!chat.matches_quick_view(ConversationSelection::Workspace(1)));

    let timeline = &mut chat.workspace_messages[1];
    timeline.messages.push(Message {
        name: "Tove".into(),
        time: "Now".into(),
        text: "@Casey Marlowe can you review this?".into(),
        color: theme::brand(),
        own: false,
        attachment: None,
        day_divider_before: false,
        reply_context: None,
        reply_context_extra: 0,
        file_chips: Vec::new(),
        rich: None,
        reactions: Vec::new(),
    });
    timeline.local_message_ids.push(None);
    chat.quick_view = ChatQuickView::Mentions;
    assert!(!chat.matches_quick_view(ConversationSelection::Workspace(0)));
    assert!(chat.matches_quick_view(ConversationSelection::Workspace(1)));
}

#[test]
fn remote_mentions_quick_view_uses_structured_current_user_metadata() {
    let mut chat = ChatState::default();
    let misleading_local_name =
        format!("@{} appears only in message text", workspace().current_user);
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:structured-mention@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Structured mention",
            "AL",
            false,
            vec![RemoteChatMessage::for_test_with_current_user_mention(
                "Ada Lovelace",
                "2026-11-06T10:30:00Z",
                "A provider-owned mention that does not need rendered @name text",
                false,
                true,
                false,
            )],
        ),
        RemoteChatConversation::for_test(
            "19:same-name-different-user@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Same display name",
            "GH",
            false,
            vec![RemoteChatMessage::for_test_with_current_user_mention(
                "Grace Hopper",
                "2026-11-06T10:31:00Z",
                misleading_local_name,
                false,
                false,
                false,
            )],
        ),
    ]));

    chat.quick_view = ChatQuickView::Mentions;

    assert!(chat.matches_quick_view(ConversationSelection::Remote(0)));
    assert!(
        !chat.matches_quick_view(ConversationSelection::Remote(1)),
        "a matching rendered display name cannot substitute for Graph's structured recipient ID"
    );
}

#[test]
fn remote_mention_metadata_follows_selected_history_replacement_and_session_clear() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:mention-lifecycle@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Mention lifecycle",
            "AL",
            false,
            vec![RemoteChatMessage::for_test_with_current_user_mention(
                "Ada Lovelace",
                "2026-11-06T10:30:00Z",
                "Initial current-user mention",
                false,
                true,
                false,
            )],
        ),
    ]));
    chat.quick_view = ChatQuickView::Mentions;
    assert!(chat.matches_quick_view(ConversationSelection::Remote(0)));

    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:mention-lifecycle@thread.v2",
            vec![RemoteChatMessage::for_test_with_current_user_mention(
                "Ada Lovelace",
                "2026-11-06T10:31:00Z",
                "Replacement history without a current-user mention",
                false,
                false,
                false,
            )],
        ))
    );
    assert!(
        !chat.matches_quick_view(ConversationSelection::Remote(0)),
        "the selected history replacement owns the mention predicate"
    );

    chat.clear_remote_projection();
    assert!(chat.remote_chats.is_empty());
    assert!(!chat.matches_quick_view(ConversationSelection::Remote(0)));
}

#[test]
fn shared_and_recap_tabs_reflect_the_selected_message_history() {
    let messages = seeded_messages();
    let shared = shared_items(&messages);
    let recap = recap_for(&messages);

    assert_eq!(shared.len(), 3);
    assert!(shared.iter().any(|item| item.title == "layout-mockup.png"));
    assert!(shared.iter().any(|item| item.kind == "Link"));
    assert_eq!(recap.message_count, messages.len());
    assert_eq!(recap.shared_item_count, shared.len());
    assert_eq!(recap.participants, "You, Wren Castellanos");
    let last_time = &messages.last().expect("seeded messages").time;
    assert_eq!(recap.latest_activity, format!("You · {last_time}"));
}

#[test]
fn sending_a_message_requests_scroll_to_the_latest_content() {
    let mut chat = ChatState::default();
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Ship it".into();

    chat.send();

    assert!(chat.scroll_to_bottom_requested);
}

#[test]
fn switching_conversations_closes_shared_previews() {
    let mut chat = ChatState {
        shared_preview: Some(MessageAttachment::LayoutMockup),
        link_preview: Some(LinkPreview::resource("Architecture assessment")),
        ..Default::default()
    };

    chat.select_workspace_conversation(1);

    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());
}

#[test]
fn switching_conversations_closes_attachment_pickers_but_keeps_their_draft_state() {
    let mut chat = ChatState::default();
    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    composer.draft = "Keep this message".into();
    composer.pending_attachment = Some(MessageAttachment::Spreadsheet);
    composer.attachment_picker_open = true;

    chat.select_workspace_conversation(1);

    assert_eq!(chat.selected, ConversationSelection::Workspace(1));
    assert!(
        !chat
            .selected_composer_mut()
            .expect("test selection has a composer")
            .attachment_picker_open
    );

    chat.select_workspace_conversation(0);
    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    assert_eq!(composer.draft, "Keep this message");
    assert_eq!(
        composer.pending_attachment,
        Some(MessageAttachment::Spreadsheet)
    );
    assert!(!composer.attachment_picker_open);
}

#[test]
fn leaving_chat_closes_attachment_pickers_for_every_conversation() {
    let mut chat = ChatState::default();
    chat.create_local_conversation("Design review".into(), String::new());
    chat.favorite_composers[0].attachment_picker_open = true;
    chat.workspace_composers[0].attachment_picker_open = true;
    chat.local_conversations[0].composer.attachment_picker_open = true;

    chat.dismiss_contextual_overlays();

    assert!(
        chat.favorite_composers
            .iter()
            .all(|composer| !composer.attachment_picker_open)
    );
    assert!(
        chat.workspace_composers
            .iter()
            .all(|composer| !composer.attachment_picker_open)
    );
    assert!(
        chat.local_conversations
            .iter()
            .all(|conversation| !conversation.composer.attachment_picker_open)
    );
}

#[test]
fn escape_closes_chat_dialogs_without_changing_the_selected_conversation() {
    let mut chat = ChatState {
        new_chat_open: true,
        new_chat_recipient: "Design review".into(),
        ..Default::default()
    };
    let selected = chat.selected;
    let mut toast = ToastState::default();

    with_escape_press(|ctx| chat.draw_new_chat_dialog(ctx, &mut toast));

    assert!(!chat.new_chat_open);
    assert_eq!(chat.selected, selected);
    assert_eq!(chat.new_chat_recipient, "Design review");

    chat.shared_preview = Some(MessageAttachment::LayoutMockup);
    with_escape_press(|ctx| chat.draw_shared_preview(ctx));

    assert!(chat.shared_preview.is_none());
    assert_eq!(chat.selected, selected);

    assert!(chat.create_local_conversation("Design review".into(), String::new()));
    let local_id = chat.local_conversations[0].id;
    let selected = chat.selected;
    chat.begin_local_conversation_rename(local_id);
    with_escape_press(|ctx| chat.draw_local_conversation_editor(ctx, &mut toast));
    assert!(chat.editing_local_conversation.is_none());
    assert_eq!(chat.selected, selected);

    chat.begin_local_conversation_removal(local_id);
    with_escape_press(|ctx| chat.draw_local_conversation_removal(ctx, &mut toast));
    assert!(chat.removing_local_conversation.is_none());
    assert_eq!(chat.selected, selected);
}

#[test]
fn chat_dialogs_and_attachment_picker_claim_keyboard_priority() {
    let mut chat = ChatState::default();

    assert!(!chat.has_keyboard_modal());
    chat.open_new_chat();
    assert!(chat.has_keyboard_modal());
    chat.dismiss_contextual_overlays();
    assert!(!chat.has_keyboard_modal());

    assert!(chat.create_local_conversation("Design review".into(), String::new()));
    let local_id = chat.local_conversations[0].id;
    chat.begin_local_conversation_rename(local_id);
    assert!(chat.has_keyboard_modal());
    chat.dismiss_contextual_overlays();
    assert!(!chat.has_keyboard_modal());

    chat.begin_local_conversation_removal(local_id);
    assert!(chat.has_keyboard_modal());
    chat.dismiss_contextual_overlays();
    assert!(!chat.has_keyboard_modal());

    chat.link_preview = Some(LinkPreview::resource("Architecture assessment"));
    assert!(chat.has_keyboard_modal());
    chat.link_preview = None;
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .attachment_picker_open = true;
    assert!(chat.has_keyboard_modal());
}

#[test]
fn every_conversation_selection_path_resets_transient_chat_state() {
    let mut chat = ChatState {
        tab: ChatTab::Notes,
        query: "hidden conversation".into(),
        chat_search_open: true,
        quick_view: ChatQuickView::Drafts,
        shared_preview: Some(MessageAttachment::LayoutMockup),
        link_preview: Some(LinkPreview::resource("Architecture assessment")),
        ..Default::default()
    };

    chat.select_favorite_conversation(0);
    assert_eq!(chat.selected, ConversationSelection::Favorite(0));
    assert_eq!(chat.tab, ChatTab::Chat);
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());

    chat.tab = ChatTab::Recap;
    chat.open_attachment_preview(MessageAttachment::Spreadsheet);
    chat.select_search_result(super::super::ChatSearchTarget::Workspace(1));
    assert_eq!(chat.selected, ConversationSelection::Workspace(1));
    assert_eq!(chat.tab, ChatTab::Chat);
    assert_eq!(chat.quick_view, ChatQuickView::All);
    assert!(chat.query.is_empty());
    assert!(!chat.chat_search_open);
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());

    chat.tab = ChatTab::Storyline;
    chat.open_attachment_preview(MessageAttachment::LayoutMockup);
    chat.create_local_conversation("Design review".into(), String::new());
    assert_eq!(chat.selected, ConversationSelection::Local(0));
    assert_eq!(chat.tab, ChatTab::Chat);
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());

    chat.tab = ChatTab::Notes;
    chat.open_attachment_preview(MessageAttachment::Spreadsheet);
    let local_conversation_id = chat.local_conversations[0].id;
    chat.select_search_result(super::super::ChatSearchTarget::Local(local_conversation_id));
    assert_eq!(chat.tab, ChatTab::Chat);
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());
}

#[test]
fn sidebar_renders_pinned_and_unpinned_local_conversations_in_their_regions() {
    let mut chat = ChatState::default();
    let mut toast = ToastState::default();
    for index in 0..20 {
        chat.create_local_conversation(format!("Review group {index}"), String::new());
    }
    let pinned_id = chat.local_conversations[5].id;
    assert_eq!(
        chat.set_local_conversation_pinned(pinned_id, true),
        Ok(true)
    );

    render_headlessly(|ui| chat.draw_sidebar(ui, &mut toast));

    assert_eq!(chat.local_conversations.len(), 20);
    assert_eq!(chat.pinned_local_conversation_ids, [pinned_id]);
    assert_eq!(chat.selected, ConversationSelection::Local(19));
}

#[test]
fn inline_attachment_selection_uses_the_existing_local_preview() {
    let mut chat = ChatState::default();

    chat.open_attachment_preview(MessageAttachment::Spreadsheet);

    assert_eq!(chat.shared_preview, Some(MessageAttachment::Spreadsheet));
}

#[test]
fn escape_closes_the_attachment_picker_without_removing_the_pending_asset() {
    let mut chat = ChatState::default();
    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    composer.attachment_picker_open = true;
    composer.pending_attachment = Some(MessageAttachment::Spreadsheet);
    let mut toast = ToastState::default();

    with_escape_press(|ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            chat.draw_message_tab(ui, &mut toast);
        });
    });

    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    assert!(!composer.attachment_picker_open);
    assert_eq!(
        composer.pending_attachment,
        Some(MessageAttachment::Spreadsheet)
    );
}

#[test]
fn shared_chat_link_opens_the_local_link_inspector() {
    let mut chat = ChatState::default();
    let mut toast = ToastState::default();
    let message_index = chat
        .selected_messages()
        .iter()
        .position(|message| message.text.contains("://"))
        .expect("seeded chat link");

    chat.handle_message_action(message_index, MessageAction::SelectLink, &mut toast);

    assert!(chat.link_preview.is_some());
}

#[test]
fn empty_chat_has_an_honest_recap_and_no_shared_items() {
    let recap = recap_for(&[]);

    assert_eq!(recap.message_count, 0);
    assert_eq!(recap.participants, "No participants yet");
    assert_eq!(recap.shared_item_count, 0);
    assert_eq!(recap.latest_activity, "No messages yet");
}

#[test]
fn requested_local_call_tracks_the_selected_participant_and_mode() {
    let mut chat = ChatState::default();

    chat.request_call("Wren Castellanos".into(), CallMode::Video);

    assert_eq!(
        chat.take_requested_call(),
        Some(("Wren Castellanos".into(), CallMode::Video))
    );
    assert_eq!(chat.take_requested_call(), None);
    assert_eq!(CallMode::Audio.label(), "Audio");
}

#[test]
fn requested_local_calls_are_available_to_the_workspace_shell_once() {
    let mut chat = ChatState::default();
    chat.request_call("Wren Castellanos".into(), CallMode::Audio);

    assert_eq!(
        chat.take_requested_call(),
        Some(("Wren Castellanos".into(), CallMode::Audio))
    );
    assert_eq!(chat.take_requested_call(), None);
}
