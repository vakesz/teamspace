use super::super::persistence::{StoredChat, StoredConversation, StoredMessage};
use super::super::{
    ChatAction, ChatState, ConversationSelection, LinkPreview, LocalMessageMutationError,
    MAX_MESSAGE_TEXT_BYTES, SendOutcome, seeded_messages,
};
use super::{remote_projection, send_local_text};
use crate::data::workspace::workspace;
use crate::shared::model::{ChatTab, MessageAttachment};
use crate::shared::reaction::ReactionKind;

#[test]
fn seeded_chat_preserves_reference_attachments_and_reply_context() {
    let messages = seeded_messages();

    assert!(
        messages
            .iter()
            .any(|message| message.attachment == Some(MessageAttachment::LayoutMockup))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.attachment == Some(MessageAttachment::Spreadsheet))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.reply_context.is_some())
    );
    assert!(messages.iter().any(|message| !message.reactions.is_empty()));
}

#[test]
fn durable_chat_projection_round_trips_user_state_without_restoring_ui_state() {
    let mut chat = ChatState::default();
    chat.workspace_composers[0].draft = "Primary draft".into();
    chat.workspace_composers[0].pending_attachment = Some(MessageAttachment::LayoutMockup);
    chat.workspace_notes[0] = "Follow up with the team".into();
    assert_eq!(
        chat.toggle_message_reaction(0, ReactionKind::Heart),
        Some(true)
    );
    chat.select_workspace_conversation(1);
    chat.workspace_composers[1].draft = "Second chat draft".into();
    chat.create_local_conversation("Design review".into(), "Can we review this today?".into());
    chat.local_conversations[0].notes = "Bring the prototype".into();
    chat.local_conversations[0].composer.draft = "Saved local draft".into();
    chat.local_conversations[0].composer.pending_attachment = Some(MessageAttachment::Spreadsheet);
    chat.query = "transient search".into();
    chat.chat_search_open = true;
    chat.tab = ChatTab::Notes;
    chat.new_chat_open = true;
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .attachment_picker_open = true;

    let restored = ChatState::from_stored(chat.stored());

    assert_eq!(restored.selected, ConversationSelection::Workspace(0));
    assert_eq!(restored.workspace_composers[0].draft, "Primary draft");
    assert_eq!(
        restored.workspace_composers[0].pending_attachment,
        Some(MessageAttachment::LayoutMockup)
    );
    assert_eq!(restored.workspace_notes[0], "Follow up with the team");
    assert!(
        restored.workspace_messages[0]
            .messages
            .iter()
            .any(|message| message.reactions.iter().any(|reaction| {
                reaction.kind == ReactionKind::Heart && reaction.reacted_by_current_user
            }))
    );
    assert!(restored.workspace_read[1]);
    assert_eq!(restored.workspace_composers[1].draft, "Second chat draft");
    assert_eq!(restored.local_conversations.len(), 1);
    assert_eq!(restored.local_conversations[0].id, 1);
    assert_eq!(restored.local_conversations[0].notes, "Bring the prototype");
    assert_eq!(
        restored.local_conversations[0].composer.pending_attachment,
        Some(MessageAttachment::Spreadsheet)
    );
    assert_eq!(
        restored.local_conversations[0].messages.messages[0].text,
        "Can we review this today?"
    );
    assert!(restored.query.is_empty());
    assert!(!restored.chat_search_open);
    assert_eq!(restored.tab, ChatTab::Chat);
    assert!(!restored.new_chat_open);
    assert!(
        !restored.local_conversations[0]
            .composer
            .attachment_picker_open
    );
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn stored_seed_state_resolves_by_fixture_id_not_position() {
    let mut stored = StoredChat::default();
    let target = &workspace().chat.conversations[1];
    stored.conversations.insert(
        target.id.clone(),
        StoredConversation {
            messages: vec![StoredMessage {
                local_id: None,
                name: "Tove".into(),
                time: "Now".into(),
                text: "Saved against the second fixture chat".into(),
                color_rgb: [120, 130, 201],
                own: false,
                attachment: None,
                day_divider_before: false,
                reply_context: None,
                reactions: Vec::new(),
            }],
            notes: "Fixture-ID note".into(),
            draft: "Fixture-ID draft".into(),
            pending_attachment: None,
            read: true,
        },
    );

    let restored = ChatState::from_stored(stored);

    assert_eq!(
        restored.workspace_messages[1].messages[0].text,
        "Saved against the second fixture chat"
    );
    assert_eq!(restored.workspace_notes[1], "Fixture-ID note");
    assert_eq!(restored.workspace_composers[1].draft, "Fixture-ID draft");
    assert!(restored.workspace_read[1]);
    assert!(!restored.workspace_messages[0].messages.is_empty());
}

#[test]
fn only_durable_chat_mutations_mark_the_projection_dirty() {
    let mut chat = ChatState::default();

    chat.open_new_chat();
    assert!(!chat.is_persistence_dirty());
    chat.dismiss_contextual_overlays();
    assert!(!chat.is_persistence_dirty());

    chat.select_workspace_conversation(1);
    assert!(chat.is_persistence_dirty());
    chat.mark_persisted();
    assert!(!chat.is_persistence_dirty());

    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Draft to send".into();
    chat.send();
    assert!(chat.is_persistence_dirty());
}

#[test]
fn reactions_toggle_for_the_current_user_without_leaking_between_conversations() {
    let mut chat = ChatState::default();

    assert_eq!(
        chat.toggle_message_reaction(0, ReactionKind::Like),
        Some(false)
    );
    assert!(chat.selected_messages()[0].reactions.is_empty());

    chat.selected = ConversationSelection::Workspace(1);
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "A second conversation message".into();
    chat.send();
    assert_eq!(
        chat.toggle_message_reaction(0, ReactionKind::Heart),
        Some(true)
    );
    assert_eq!(chat.selected_messages()[0].reactions[0].count, 1);
    assert!(chat.selected_messages()[0].reactions[0].reacted_by_current_user);

    chat.selected = ConversationSelection::Workspace(0);
    assert!(chat.selected_messages()[0].reactions.is_empty());
}

#[test]
fn local_sad_and_angry_reactions_round_trip_through_chat_storage() {
    let mut chat = ChatState::default();
    assert_eq!(
        chat.toggle_message_reaction(0, ReactionKind::Sad),
        Some(true)
    );
    assert_eq!(
        chat.toggle_message_reaction(0, ReactionKind::Angry),
        Some(true)
    );

    let restored = ChatState::from_stored(chat.stored());
    let reactions = &restored.selected_messages()[0].reactions;
    assert!(
        reactions
            .iter()
            .any(|reaction| reaction.kind == ReactionKind::Sad && reaction.reacted_by_current_user)
    );
    assert!(reactions.iter().any(|reaction| {
        reaction.kind == ReactionKind::Angry && reaction.reacted_by_current_user
    }));
}

#[test]
fn text_sends_keep_local_feedback_and_emit_one_transient_action() {
    let mut chat = ChatState::default();
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "  Ready for review  ".into();
    chat.send();

    let last = chat.selected_messages().last().expect("a sent message");
    assert_eq!(last.name, "You");
    assert_eq!(last.text, "Ready for review");
    assert!(last.own);
    assert!(
        chat.selected_composer_mut()
            .expect("test selection has a composer")
            .draft
            .is_empty()
    );
    let stored = chat.stored();
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::SendLocalMessage {
            chat_id: workspace().chat.primary_conversation().id.clone(),
            body: "Ready for review".into(),
        }]
    );
    assert!(chat.take_actions().is_empty());

    let mut restored = ChatState::from_stored(stored);
    assert!(restored.take_actions().is_empty());
}

#[test]
fn locally_authored_messages_edit_with_a_stable_persisted_identity() {
    let mut chat = ChatState::default();
    let local_message_id = send_local_text(&mut chat, "Ready for review");
    let _ = chat.take_actions();

    assert_eq!(
        chat.edit_local_message(local_message_id, "  Updated for review  "),
        Ok(())
    );
    assert_eq!(
        chat.selected_messages()
            .last()
            .map(|message| message.text.as_str()),
        Some("Updated for review")
    );
    assert!(chat.take_actions().is_empty());

    let stored = chat.stored();
    assert!(stored.next_local_message_id > local_message_id);
    let stored_message = stored
        .conversations
        .get(&workspace().chat.primary_conversation().id)
        .and_then(|conversation| conversation.messages.last())
        .expect("edited message must remain in the primary conversation");
    assert_eq!(stored_message.local_id, Some(local_message_id));
    assert_eq!(stored_message.text, "Updated for review");

    let restored = ChatState::from_stored(stored);
    assert_eq!(
        restored
            .selected_messages()
            .last()
            .map(|message| message.text.as_str()),
        Some("Updated for review")
    );
    assert_eq!(
        restored
            .timeline_for(ConversationSelection::Workspace(0))
            .and_then(|timeline| timeline.local_id_at(restored.selected_messages().len() - 1)),
        Some(local_message_id)
    );
}

#[test]
fn invalid_local_message_edits_are_atomic() {
    let mut chat = ChatState::default();
    let local_message_id = send_local_text(&mut chat, "Keep this draft");
    let before = chat.stored();

    assert_eq!(
        chat.edit_local_message(local_message_id, "   \n\t"),
        Err(LocalMessageMutationError::Empty)
    );
    assert_eq!(chat.stored(), before);

    let too_long = "x".repeat(MAX_MESSAGE_TEXT_BYTES + 1);
    assert_eq!(
        chat.edit_local_message(local_message_id, &too_long),
        Err(LocalMessageMutationError::TooLong)
    );
    assert_eq!(chat.stored(), before);

    chat.next_local_message_id = local_message_id;
    let original_text = chat
        .selected_messages()
        .last()
        .expect("local message must remain")
        .text
        .clone();
    assert_eq!(
        chat.edit_local_message(local_message_id, "Would violate the snapshot allocator"),
        Err(LocalMessageMutationError::NotPersistable)
    );
    assert_eq!(
        chat.selected_messages()
            .last()
            .map(|message| message.text.as_str()),
        Some(original_text.as_str())
    );
}

#[test]
fn deleting_a_local_message_clears_its_search_and_never_reuses_its_id() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Retrospective".into(), String::new()));
    let local_conversation_id = chat.local_conversations[0].id;
    let local_message_id = send_local_text(&mut chat, "Needle-only local content");
    let message_index = chat.selected_messages().len() - 1;
    assert_eq!(
        chat.toggle_message_reaction(message_index, ReactionKind::Heart),
        Some(true)
    );
    assert!(chat.search_results("needle-only").iter().any(
        |result| result.target == super::super::ChatSearchTarget::Local(local_conversation_id)
    ));

    let next_local_message_id = chat.next_local_message_id;
    chat.next_local_message_id = 0;
    assert_eq!(
        chat.delete_local_message(local_message_id),
        Err(LocalMessageMutationError::NotPersistable)
    );
    assert_eq!(chat.selected_messages().len(), 1);
    chat.next_local_message_id = next_local_message_id;
    assert_eq!(chat.delete_local_message(local_message_id), Ok(()));
    assert!(chat.selected_messages().is_empty());
    assert!(chat.search_results("needle-only").is_empty());
    assert!(
        chat.timeline_for(ConversationSelection::Local(0))
            .is_some_and(|timeline| timeline.index_for_local_id(local_message_id).is_none())
    );

    let next_local_message_id = send_local_text(&mut chat, "A later local message");
    assert!(next_local_message_id > local_message_id);
    let mut restored = ChatState::from_stored(chat.stored());
    restored.select_search_result(super::super::ChatSearchTarget::Local(local_conversation_id));
    assert_eq!(restored.selected_messages().len(), 1);
    assert_eq!(
        restored.selected_messages()[0].text,
        "A later local message"
    );
}

#[test]
fn fixture_and_legacy_messages_without_local_ids_remain_immutable() {
    let chat = ChatState::from_stored(ChatState::default().stored());
    assert!(
        chat.timeline_for(ConversationSelection::Workspace(0))
            .is_some_and(|timeline| timeline.local_message_ids.iter().all(Option::is_none))
    );

    let mut chat = chat;
    assert_eq!(
        chat.edit_local_message(1, "Attempted fixture edit"),
        Err(LocalMessageMutationError::Missing)
    );
    assert_eq!(
        chat.delete_local_message(1),
        Err(LocalMessageMutationError::Missing)
    );
    assert_eq!(chat.selected_messages().len(), seeded_messages().len());
    assert_eq!(send_local_text(&mut chat, "New mutable local message"), 1);
}

#[test]
fn exhausted_local_identifiers_do_not_wrap_or_reuse_values() {
    let mut chat = ChatState {
        next_local_conversation_id: u64::MAX,
        ..Default::default()
    };
    let next_message_id = chat.next_local_message_id;
    assert!(!chat.create_local_conversation("Exhausted".into(), "Message".into()));
    assert!(chat.local_conversations.is_empty());
    assert_eq!(chat.next_local_conversation_id, u64::MAX);
    assert_eq!(chat.next_local_message_id, next_message_id);

    let mut message_exhausted = ChatState {
        next_local_message_id: u64::MAX,
        ..Default::default()
    };
    message_exhausted
        .selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Do not reuse IDs".into();
    message_exhausted.send();
    assert_eq!(
        message_exhausted.selected_messages().len(),
        seeded_messages().len()
    );
    assert_eq!(
        message_exhausted
            .selected_composer_mut()
            .expect("test selection has a composer")
            .draft,
        "Do not reuse IDs",
        "failed allocation must leave the user draft recoverable"
    );
    assert!(message_exhausted.take_actions().is_empty());
}

#[test]
fn rejected_local_message_creates_preserve_drafts_and_state() {
    let mut chat = ChatState::default();
    let initial_message_count = chat.selected_messages().len();
    let initial_next_message_id = chat.next_local_message_id;
    let too_long = "x".repeat(MAX_MESSAGE_TEXT_BYTES + 1);
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = too_long.clone();

    assert!(matches!(
        chat.send(),
        SendOutcome::Rejected(LocalMessageMutationError::TooLong)
    ));
    assert_eq!(chat.selected_messages().len(), initial_message_count);
    assert_eq!(chat.next_local_message_id, initial_next_message_id);
    assert_eq!(
        chat.selected_composer_mut()
            .expect("test selection has a composer")
            .draft,
        too_long
    );
    assert!(chat.take_actions().is_empty());

    let before_conversations = chat.local_conversations.len();
    let before_conversation_id = chat.next_local_conversation_id;
    let before_message_id = chat.next_local_message_id;
    assert!(
        !chat.create_local_conversation("Oversized".into(), "x".repeat(MAX_MESSAGE_TEXT_BYTES + 1))
    );
    assert_eq!(chat.local_conversations.len(), before_conversations);
    assert_eq!(chat.next_local_conversation_id, before_conversation_id);
    assert_eq!(chat.next_local_message_id, before_message_id);
}

#[test]
fn prospective_create_validation_rejects_duplicate_ids_without_mutating_state() {
    let mut chat = ChatState::default();
    let existing_local_id = send_local_text(&mut chat, "Existing local message");
    chat.next_local_message_id = existing_local_id;
    let original_count = chat.selected_messages().len();
    let original_conversations = chat.local_conversations.len();

    assert!(matches!(chat.send(), SendOutcome::NoContent));
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "Would duplicate an ID".into();
    assert!(matches!(
        chat.send(),
        SendOutcome::Rejected(LocalMessageMutationError::NotPersistable)
    ));
    assert_eq!(chat.selected_messages().len(), original_count);
    assert_eq!(chat.local_conversations.len(), original_conversations);
    assert_eq!(chat.next_local_message_id, existing_local_id);
    assert_eq!(
        chat.selected_composer_mut()
            .expect("test selection has a composer")
            .draft,
        "Would duplicate an ID"
    );

    assert!(!chat.create_local_conversation(
        "Duplicate local chat".into(),
        "Would duplicate an ID in a new chat".into()
    ));
    assert_eq!(chat.local_conversations.len(), original_conversations);
    assert_eq!(chat.next_local_message_id, existing_local_id);
}

#[test]
fn sending_an_attachment_without_text_keeps_the_asset() {
    let mut chat = ChatState::default();
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .pending_attachment = Some(MessageAttachment::Spreadsheet);
    chat.send();

    let last = chat
        .selected_messages()
        .last()
        .expect("an attachment message");
    assert_eq!(last.text, "Shared an attachment");
    assert_eq!(last.attachment, Some(MessageAttachment::Spreadsheet));
    assert!(
        chat.selected_composer_mut()
            .expect("test selection has a composer")
            .pending_attachment
            .is_none()
    );
    assert!(chat.take_actions().is_empty());
}

#[test]
fn creating_a_local_conversation_selects_it_and_keeps_the_first_message() {
    let mut chat = ChatState::default();
    chat.create_local_conversation("Design review".into(), "Can we review this today?".into());

    assert_eq!(chat.selected, ConversationSelection::Local(0));
    assert_eq!(chat.local_conversations[0].initials, "DR");
    assert_eq!(
        chat.selected_messages()[0].text,
        "Can we review this today?"
    );
    assert_eq!(
        chat.take_actions(),
        vec![ChatAction::SendLocalMessage {
            chat_id: "local:1".into(),
            body: "Can we review this today?".into(),
        }]
    );
}

#[test]
fn starting_a_new_chat_requires_a_recipient_and_trims_the_first_message() {
    let mut chat = ChatState::default();

    assert_eq!(chat.start_new_chat(), Err(()));
    assert!(chat.local_conversations.is_empty());

    chat.new_chat_recipient = "  Design review  ".into();
    chat.new_chat_message = "  Can we review this today?  ".into();
    assert_eq!(chat.start_new_chat(), Ok(()));
    assert_eq!(chat.selected, ConversationSelection::Local(0));
    assert_eq!(chat.local_conversations[0].name, "Design review");
    assert_eq!(
        chat.selected_messages()[0].text,
        "Can we review this today?"
    );
}

#[test]
fn opening_a_new_chat_clears_stale_values_and_requests_recipient_focus() {
    let mut chat = ChatState {
        new_chat_recipient: "Previous recipient".into(),
        new_chat_message: "Previous message".into(),
        shared_preview: Some(MessageAttachment::Spreadsheet),
        link_preview: Some(LinkPreview::resource("Architecture assessment")),
        ..Default::default()
    };

    chat.open_new_chat();

    assert!(chat.new_chat_open);
    assert!(chat.new_chat_focus_requested);
    assert!(chat.new_chat_recipient.is_empty());
    assert!(chat.new_chat_message.is_empty());
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());
}

#[test]
fn dismissing_contextual_overlays_keeps_the_current_message_draft() {
    let mut chat = ChatState {
        new_chat_open: true,
        new_chat_focus_requested: true,
        shared_preview: Some(MessageAttachment::Spreadsheet),
        link_preview: Some(LinkPreview::resource("Architecture assessment")),
        ..Default::default()
    };
    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    composer.draft = "Keep this draft".into();
    composer.pending_attachment = Some(MessageAttachment::LayoutMockup);
    composer.attachment_picker_open = true;

    chat.dismiss_contextual_overlays();

    assert!(!chat.new_chat_open);
    assert!(!chat.new_chat_focus_requested);
    assert!(chat.shared_preview.is_none());
    assert!(chat.link_preview.is_none());
    let composer = chat
        .selected_composer_mut()
        .expect("test selection has a composer");
    assert_eq!(composer.draft, "Keep this draft");
    assert_eq!(
        composer.pending_attachment,
        Some(MessageAttachment::LayoutMockup)
    );
    assert!(!composer.attachment_picker_open);
}

#[test]
fn workspace_conversations_keep_distinct_message_histories() {
    let mut chat = ChatState::default();
    let primary_count = chat.selected_messages().len();

    chat.selected = ConversationSelection::Workspace(1);
    assert!(chat.selected_messages().is_empty());

    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "A message for this chat only".into();
    chat.send();
    assert_eq!(chat.selected_messages().len(), 1);

    chat.selected = ConversationSelection::Workspace(0);
    assert_eq!(chat.selected_messages().len(), primary_count);
}

#[test]
fn notes_are_scoped_to_the_selected_conversation() {
    let mut chat = ChatState::default();
    chat.selected_notes_mut()
        .expect("test selection has notes")
        .push_str("Follow up with the team");

    chat.selected = ConversationSelection::Workspace(1);
    assert!(
        chat.selected_notes_mut()
            .expect("test selection has notes")
            .is_empty()
    );

    chat.selected = ConversationSelection::Workspace(0);
    assert_eq!(
        chat.selected_notes_mut().expect("test selection has notes"),
        "Follow up with the team"
    );
}

#[test]
fn favorite_conversations_are_selectable_and_keep_their_own_state() {
    let mut chat = ChatState::default();
    let primary_count = chat.selected_messages().len();

    chat.select_favorite_conversation(0);
    assert_eq!(chat.selected, ConversationSelection::Favorite(0));
    assert!(chat.selected_messages().is_empty());
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = "A favorite-only update".into();
    chat.send();
    chat.selected_notes_mut()
        .expect("test selection has notes")
        .push_str("Favorite-only note");

    assert_eq!(chat.selected_messages()[0].text, "A favorite-only update");
    chat.selected = ConversationSelection::Workspace(0);
    assert_eq!(chat.selected_messages().len(), primary_count);
    assert!(
        chat.selected_notes_mut()
            .expect("test selection has notes")
            .is_empty()
    );

    chat.selected = ConversationSelection::Favorite(0);
    assert_eq!(
        chat.selected_notes_mut().expect("test selection has notes"),
        "Favorite-only note"
    );
}

#[test]
fn initially_open_chat_is_read_and_opening_another_unread_chat_marks_it_read() {
    let mut chat = ChatState::default();

    assert!(!chat.is_unread(ConversationSelection::Workspace(0)));
    assert!(chat.is_unread(ConversationSelection::Workspace(1)));

    chat.select_workspace_conversation(1);

    assert!(!chat.is_unread(ConversationSelection::Workspace(1)));
}

#[test]
fn marking_all_chats_read_preserves_conversation_drafts() {
    let mut chat = ChatState::default();
    chat.workspace_composers[0].draft = "Keep this draft".into();
    chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Unread Microsoft 365 message",
    ));

    assert!(chat.mark_all_conversations_read() > 0);
    assert!(
        crate::data::workspace::workspace()
            .chat
            .favorites
            .iter()
            .enumerate()
            .all(|(index, _)| !chat.is_unread(ConversationSelection::Favorite(index)))
    );
    assert!(
        crate::data::workspace::workspace()
            .chat
            .conversations
            .iter()
            .enumerate()
            .all(|(index, _)| !chat.is_unread(ConversationSelection::Workspace(index)))
    );
    assert!(
        !chat
            .remote_chats
            .get(0)
            .expect("the remote chat must remain projected")
            .is_unread()
    );
    assert_eq!(chat.workspace_composers[0].draft, "Keep this draft");
    assert_eq!(chat.mark_all_conversations_read(), 0);
}

#[test]
fn undoing_a_local_message_delete_restores_it_in_place_and_persists_again() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Retrospective".into(), String::new()));
    let first = send_local_text(&mut chat, "First");
    let second = send_local_text(&mut chat, "Second");
    let third = send_local_text(&mut chat, "Third");
    assert_eq!(chat.delete_local_message(second), Ok(()));
    chat.mark_persisted();

    assert!(chat.undo_delete_local_message());
    let texts: Vec<_> = chat
        .selected_messages()
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(texts, ["First", "Second", "Third"]);
    assert!(chat.is_persistence_dirty());
    let timeline = chat
        .timeline_for(ConversationSelection::Local(0))
        .expect("timeline");
    assert_eq!(timeline.index_for_local_id(first), Some(0));
    assert_eq!(timeline.index_for_local_id(second), Some(1));
    assert_eq!(timeline.index_for_local_id(third), Some(2));

    // One undo per delete: the buffer is consumed.
    assert!(!chat.undo_delete_local_message());
}

#[test]
fn undo_is_dropped_when_its_conversation_is_removed() {
    let mut chat = ChatState::default();
    assert!(chat.create_local_conversation("Retrospective".into(), String::new()));
    let id = chat.local_conversations[0].id;
    let message = send_local_text(&mut chat, "Gone soon");
    assert_eq!(chat.delete_local_message(message), Ok(()));
    assert_eq!(chat.remove_local_conversation(id), Ok(()));
    assert!(!chat.undo_delete_local_message());
}

#[test]
fn the_rail_unread_chat_count_follows_read_state_and_remote_projection() {
    let mut chat = ChatState::default();
    let catalog = chat.unread_chat_count();
    assert!(catalog > 0, "the preview catalog has unread chats");

    chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Unread Microsoft 365 message",
    ));
    let remote_unread = chat.remote_chats.iter().filter(|c| c.is_unread()).count();
    assert!(remote_unread > 0);
    assert_eq!(chat.unread_chat_count(), catalog + remote_unread);

    chat.mark_all_conversations_read();
    assert_eq!(chat.unread_chat_count(), 0);
}
