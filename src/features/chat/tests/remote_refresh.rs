//! Keep-Ready refresh of a selected remote chat and the `read_through`
//! unread rule. Rows from an earlier exact read stay visible, but only an
//! exact `Ready` history may unlock a reaction, quote or edit control.

use super::super::remote::RemoteChatHistoryState as State;
use super::*;

const CHAT: &str = "19:refresh@thread.v2";

fn open_with_history(message: &str) -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(CHAT, message, "Body", 0, false));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, message, "Body", 0, false,
        ))
    );
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Ready));
    chat
}

fn can_react(chat: &ChatState) -> bool {
    chat.remote_chats
        .reaction_view(0, 0, ReactionKind::Like)
        .is_some_and(|view| view.can_react)
}

#[test]
fn directory_replace_keeps_ready_when_the_newest_message_is_unchanged() {
    let mut chat = open_with_history("m1");
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m1", "Body", 0, false));
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Ready));
    assert!(chat.take_remote_chat_messages_request().is_none());
    assert!(can_react(&chat));
}

#[test]
fn newer_preview_marks_history_stale_gates_it_and_requests_a_refresh() {
    let mut chat = open_with_history("m1");
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m2", "Newer", 0, false));
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Refreshing));
    assert_eq!(
        chat.selected_messages()[0].text,
        "Body",
        "rows stay visible"
    );
    assert!(
        !can_react(&chat),
        "refreshing rows must not unlock controls"
    );
    assert!(chat.take_remote_chat_messages_request().is_some());
}

#[test]
fn exact_read_after_refresh_is_ready_again() {
    let mut chat = open_with_history("m1");
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m2", "Newer", 0, false));
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, "m2", "Newer", 0, false,
        ))
    );
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Ready));
    assert_eq!(chat.selected_messages()[0].text, "Newer");
}

#[test]
fn refresh_failure_keeps_stale_rows_gated_and_never_sticks_on_refreshing() {
    let mut chat = open_with_history("m1");
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m2", "Newer", 0, false));
    let _ = chat.take_remote_chat_messages_request();
    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::unavailable_for_test(CHAT)
    ));
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Stale));
    assert_eq!(chat.selected_messages()[0].text, "Body");
    assert!(!can_react(&chat));
    // The next directory replace retries the exact read.
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m2", "Newer", 0, false));
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Refreshing));
    assert!(chat.take_remote_chat_messages_request().is_some());
}

#[test]
fn access_denied_during_refresh_becomes_access_denied() {
    let mut chat = open_with_history("m1");
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m2", "Newer", 0, false));
    let _ = chat.take_remote_chat_messages_request();
    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::access_denied_for_test(CHAT)
    ));
    assert_eq!(
        chat.remote_chats.history_state(0),
        Some(State::AccessDenied)
    );
}

fn two_chats(second_unread_preview: &str) -> RemoteChatProjection {
    let conversation = |id: &str, message: &str| {
        RemoteChatConversation::for_test(
            id,
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Sync",
            "AL",
            true,
            vec![
                RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    message,
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Hi",
                    false,
                ),
            ],
        )
    };
    RemoteChatProjection::for_test(vec![
        conversation("19:a@thread.v2", "a1"),
        conversation("19:b@thread.v2", second_unread_preview),
    ])
}

#[test]
fn read_through_hides_only_the_message_that_was_read() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(two_chats("b1"));
    assert!(chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
    // Select B (marks read), then leave for A so B is no longer selected.
    chat.select_remote_conversation(1);
    chat.select_remote_conversation(0);
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
    chat.apply_remote_projection(two_chats("b1"));
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
    // A newer message while the provider still says unread shows the dot.
    chat.apply_remote_projection(two_chats("b2"));
    assert!(chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
}

#[test]
fn an_unknown_preview_target_stays_unread_after_a_refresh() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection("19:x@thread.v2", "No id"));
    chat.select_remote_conversation(0);
    assert!(!chat.remote_chats.get(0).is_some_and(|c| c.is_unread()));
    chat.apply_remote_projection(remote_projection("19:x@thread.v2", "No id"));
    assert!(
        chat.remote_chats.get(0).is_some_and(|c| c.is_unread()),
        "a missing target never proves the message was read"
    );
}

#[test]
fn a_newer_arrival_in_the_selected_chat_reads_as_read() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(two_chats("b1"));
    chat.select_remote_conversation(1);
    chat.apply_remote_projection(two_chats("b2"));
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
}

#[test]
fn an_arrival_in_the_selected_chat_stays_unread_while_the_window_is_unfocused() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(two_chats("b1"));
    chat.select_remote_conversation(1);
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));

    chat.set_window_focused(false);
    chat.apply_remote_projection(two_chats("b2"));
    assert!(
        chat.remote_chats.get(1).is_some_and(|c| c.is_unread()),
        "a message that arrives while away must not be acknowledged"
    );
    // Further refreshes while away keep it unread.
    chat.apply_remote_projection(two_chats("b2"));
    assert!(chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
}

#[test]
fn regaining_focus_acknowledges_what_the_open_chat_received_while_away() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(two_chats("b1"));
    chat.select_remote_conversation(1);
    chat.set_window_focused(false);
    chat.apply_remote_projection(two_chats("b2"));
    assert!(chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));

    chat.set_window_focused(true);
    chat.on_window_focus_gained();
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
    chat.apply_remote_projection(two_chats("b2"));
    assert!(!chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
}

#[test]
fn an_unfocused_arrival_in_a_chat_that_is_not_selected_stays_unread_regardless() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(two_chats("b1"));
    chat.select_remote_conversation(1);
    chat.select_remote_conversation(0);
    chat.set_window_focused(true);
    chat.apply_remote_projection(two_chats("b2"));
    assert!(chat.remote_chats.get(1).is_some_and(|c| c.is_unread()));
}

#[test]
fn explicit_selected_refresh_keeps_ready_rows_gated_and_only_refreshes_ready_history() {
    let mut chat = open_with_history("m1");
    assert!(chat.begin_selected_remote_history_refresh().is_some());
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Refreshing));
    assert_eq!(
        chat.selected_messages()[0].text,
        "Body",
        "rows stay visible"
    );
    assert!(!can_react(&chat), "refreshing rows are not actionable");
    // Already refreshing: a second event starts nothing.
    assert!(chat.begin_selected_remote_history_refresh().is_none());
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, "m1", "Body", 0, false,
        ))
    );
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Ready));
}

#[test]
fn explicit_refresh_does_nothing_for_a_loading_or_unselected_chat() {
    let mut chat = ChatState::default();
    assert!(chat.begin_selected_remote_history_refresh().is_none());
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m1", "Body", 0, false));
    chat.select_remote_conversation(0);
    assert_eq!(chat.remote_chats.history_state(0), Some(State::Loading));
    assert!(chat.begin_selected_remote_history_refresh().is_none());
}
