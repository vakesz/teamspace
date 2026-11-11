//! Opt-in read receipts: once per view, gated by preference, capability,
//! focus, exact Ready history and a provider-unread chat.

use super::*;
use crate::integrations::{Capability, RemoteCapabilities};

const CHAT: &str = "19:receipt@thread.v2";

fn caps(chat_mark_read: Capability) -> RemoteCapabilities {
    RemoteCapabilities {
        chat_mark_read,
        ..RemoteCapabilities::default()
    }
}

/// A selected chat with exact Ready history that the provider still
/// reports unread.
fn ready_unread() -> ChatState {
    let mut chat = ChatState::default();
    chat.set_send_read_receipts(true);
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m1", "Body", 0, false));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, "m1", "Body", 0, false,
        ))
    );
    chat
}

#[test]
fn fires_once_for_the_view_when_every_condition_holds() {
    let mut chat = ready_unread();
    let target = chat.take_mark_read_request(true).expect("one receipt");
    assert_eq!(
        target,
        RemoteChatMessagesProjection::for_test(CHAT, Vec::new()).chat()
    );
    assert!(chat.take_mark_read_request(true).is_none(), "once per view");
}

#[test]
fn a_new_selection_view_may_send_again() {
    let mut chat = ready_unread();
    assert!(chat.take_mark_read_request(true).is_some());
    chat.select_workspace_conversation(0);
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, "m1", "Body", 0, false,
        ))
    );
    assert!(chat.take_mark_read_request(true).is_some());
}

#[test]
fn the_preference_defaults_off_and_never_fires_when_off() {
    assert!(!ChatState::default().send_read_receipts());
    let mut chat = ready_unread();
    chat.set_send_read_receipts(false);
    assert!(chat.take_mark_read_request(true).is_none());
}

#[test]
fn an_unfocused_window_never_fires_and_does_not_burn_the_view() {
    let mut chat = ready_unread();
    assert!(chat.take_mark_read_request(false).is_none());
    assert!(chat.take_mark_read_request(true).is_some());
}

#[test]
fn it_needs_a_ready_exact_history_and_a_provider_unread_chat() {
    let mut chat = ChatState::default();
    chat.set_send_read_receipts(true);
    chat.apply_remote_projection(remote_reaction_projection(CHAT, "m1", "Body", 0, false));
    chat.select_remote_conversation(0);
    assert!(chat.take_mark_read_request(true).is_none(), "still loading");
    let _ = chat.take_remote_chat_messages_request();
    // A directory preview that is already read gives nothing to send.
    let mut read = ChatState::default();
    read.set_send_read_receipts(true);
    read.apply_remote_projection(crate::integrations::RemoteChatProjection::for_test(vec![
        crate::integrations::RemoteChatConversation::for_test(
            CHAT,
            crate::integrations::RemoteChatKind::OneOnOne,
            "Ada",
            "Hi",
            "A",
            false,
            vec![],
        ),
    ]));
    read.select_remote_conversation(0);
    let _ = read.take_remote_chat_messages_request();
    assert!(
        read.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT, "m1", "Body", 0, false,
        ))
    );
    assert!(read.take_mark_read_request(true).is_none());
}

#[test]
fn the_capability_gates_it_and_a_403_turns_it_off_for_the_session() {
    let mut chat = ready_unread();
    chat.apply_remote_capabilities(caps(Capability::NotGranted));
    assert!(!chat.read_receipts_offered());
    assert!(chat.take_mark_read_request(true).is_none());

    for offered in [Capability::Available, Capability::Unknown] {
        let mut chat = ready_unread();
        chat.apply_remote_capabilities(caps(offered));
        assert!(chat.read_receipts_offered());
        chat.mark_read_forbidden();
        assert!(!chat.read_receipts_offered());
        assert!(chat.take_mark_read_request(true).is_none());
    }
}

#[test]
fn nothing_but_the_choice_is_persisted() {
    let mut chat = ready_unread();
    assert!(chat.is_prefs_dirty());
    let _ = chat.take_mark_read_request(true);
    chat.mark_prefs_persisted();
    assert!(!chat.is_prefs_dirty(), "sending never dirties preferences");
    assert!(chat.prefs().send_read_receipts);
}
