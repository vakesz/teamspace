use super::super::{ChatState, ConversationSelection};
use super::remote_projection;
use crate::integrations::{RemoteChatMessage, RemoteChatMessagesProjection};

const CHAT_ID: &str = "19:search-open@thread.v2";

fn message(id: &str) -> RemoteChatMessage {
    RemoteChatMessage::for_test_with_id_and_current_user_mention(
        id,
        "Ada",
        "2026-11-06T10:31:00Z",
        "text",
        false,
    )
}

fn chat_target() -> crate::integrations::ProviderResourceRef {
    RemoteChatMessagesProjection::for_test(CHAT_ID, vec![message("m1")]).chat()
}

fn loaded_chat() -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Preview"));
    chat
}

#[test]
fn a_hit_opens_its_chat_by_target_and_queues_the_history_read() {
    let mut chat = loaded_chat();
    let target = chat_target();
    assert!(chat.open_remote_chat(&target, None));
    assert_eq!(chat.selected, ConversationSelection::Remote(0));
    assert_eq!(chat.take_remote_chat_messages_request(), Some(target));
}

#[test]
fn a_hit_in_a_chat_outside_the_directory_opens_nothing() {
    let mut chat = loaded_chat();
    let other = RemoteChatMessagesProjection::for_test("19:unknown@thread.v2", vec![]).chat();
    let before = chat.selected;
    assert!(!chat.open_remote_chat(&other, None));
    assert_eq!(chat.selected, before);
    assert!(chat.take_remote_chat_messages_request().is_none());
}

#[test]
fn a_missing_message_gets_one_notice_once_the_history_is_ready() {
    let mut chat = loaded_chat();
    let missing = message("not-there").target().expect("a targeted message");
    assert!(chat.open_remote_chat(&chat_target(), Some(&missing)));
    // Still loading: nothing to say yet.
    assert!(chat.take_search_focus_notice().is_none());
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![message("m1")]
        ))
    );
    assert!(chat.take_search_focus_notice().is_some());
    assert!(chat.take_search_focus_notice().is_none());
}

#[test]
fn a_loaded_message_gets_no_notice_and_stays_pending_until_drawn() {
    let mut chat = loaded_chat();
    let present = message("m1").target().expect("a targeted message");
    assert!(chat.open_remote_chat(&chat_target(), Some(&present)));
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![message("m1")]
        ))
    );
    assert!(chat.take_search_focus_notice().is_none());
    assert!(chat.search_focus.is_some());
}

#[test]
fn moving_the_selection_drops_a_pending_focus() {
    let mut chat = loaded_chat();
    let present = message("m1").target().expect("a targeted message");
    assert!(chat.open_remote_chat(&chat_target(), Some(&present)));
    chat.select_workspace_conversation(0);
    assert!(chat.search_focus.is_none());
}
