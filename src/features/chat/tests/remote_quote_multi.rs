use super::super::{ChatAction, ChatState, RemoteReconciliationResolution, RemoteSendError};
use super::remote_projection;
use crate::integrations::{
    MAX_REMOTE_QUOTE_SOURCES, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, WorkspaceIntentId,
};

const CHAT: &str = "19:multi-quote@thread.v2";

fn rows(count: usize) -> Vec<RemoteChatMessage> {
    (0..count)
        .map(|n| {
            RemoteChatMessage::for_test_with_reactions(
                format!("src{n}"),
                "Ada Lovelace",
                "2026-11-06T10:31:00Z",
                format!("Source {n}"),
                false,
                false,
                Vec::new(),
            )
        })
        .collect()
}

fn ready_chat(count: usize) -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT, "preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT,
            rows(count)
        ))
    );
    chat
}

fn conversation(chat: &mut ChatState) -> &mut super::super::remote::RemoteConversation {
    chat.remote_chats.get_mut(0).expect("selected remote chat")
}

fn history_with(chat: &mut ChatState, kept: &[usize], extra: Vec<RemoteChatMessage>) {
    let mut all: Vec<_> = rows(16)
        .into_iter()
        .enumerate()
        .filter(|(n, _)| kept.contains(n))
        .map(|(_, row)| row)
        .collect();
    all.extend(extra);
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT, all
        ))
    );
}

/// Quotes `sources`, sends "Reply", and returns the intent after an
/// ambiguous outcome.
fn uncertain_send(chat: &mut ChatState, sources: &[usize], intent: u64) -> WorkspaceIntentId {
    for source in sources {
        chat.toggle_remote_quote_source(0, *source)
            .expect("source can be quoted");
    }
    conversation(chat).draft_mut().push_str("Reply");
    chat.queue_remote_message(0).expect("quote reply queues");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::ReplyToRemoteChatMessageWithQuote { submission, .. }] => *submission,
        _ => panic!("one quote action expected"),
    };
    let intent = WorkspaceIntentId::for_test(intent);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    intent
}

fn own_reply(quoted: &[&str]) -> RemoteChatMessage {
    let triples: Vec<(&str, &str, &str)> =
        quoted.iter().map(|id| (*id, "Ada Lovelace", "p")).collect();
    RemoteChatMessage::for_test_with_quote_references("reply", "Reply", true, &triples)
}

#[test]
fn quote_toggle_caps_at_ten_and_a_removal_frees_a_slot() {
    let mut chat = ready_chat(11);
    for n in 0..MAX_REMOTE_QUOTE_SOURCES {
        chat.toggle_remote_quote_source(0, n).expect("within cap");
    }
    assert_eq!(
        chat.toggle_remote_quote_source(0, 10),
        Err(RemoteSendError::QuoteLimit)
    );
    let view = chat.remote_chats.quote_reply_view(0, 10).expect("view");
    assert!(!view.can_quote && view.limit_reached && !view.selected);
    let selected = chat.remote_chats.quote_reply_view(0, 3).expect("view");
    assert!(selected.can_quote && selected.selected);
    chat.toggle_remote_quote_source(0, 3).expect("toggle off");
    chat.toggle_remote_quote_source(0, 10).expect("slot freed");
    assert_eq!(conversation(&mut chat).quote_reply_contexts().len(), 10);
}

#[test]
fn send_carries_every_source_in_selection_order_and_forces_normal() {
    let mut chat = ready_chat(3);
    conversation(&mut chat).set_composer_importance(RemoteChatMessageImportance::Urgent);
    for n in [2, 0, 1] {
        chat.toggle_remote_quote_source(0, n).expect("quote");
    }
    assert_eq!(
        chat.remote_chats.get(0).expect("row").composer_importance(),
        RemoteChatMessageImportance::Normal
    );
    conversation(&mut chat).draft_mut().push_str("All three");
    chat.queue_remote_message(0).expect("queues");
    match chat.take_actions().as_slice() {
        [ChatAction::ReplyToRemoteChatMessageWithQuote { messages, .. }] => {
            let source = rows(3);
            let expected: Vec<_> = [2, 0, 1]
                .into_iter()
                .map(|n| source[n].target().expect("row target"))
                .collect();
            assert_eq!(messages, &expected);
        }
        _ => panic!("one quote action expected"),
    }
}

#[test]
fn cancel_restores_the_priority_chosen_before_a_multi_source_quote() {
    let mut chat = ready_chat(3);
    conversation(&mut chat).set_composer_importance(RemoteChatMessageImportance::High);
    chat.toggle_remote_quote_source(0, 0).expect("first");
    chat.toggle_remote_quote_source(0, 1).expect("second");
    assert!(conversation(&mut chat).cancel_quote_reply());
    assert_eq!(
        chat.remote_chats.get(0).expect("row").composer_importance(),
        RemoteChatMessageImportance::High
    );
}

#[test]
fn unsent_selection_drops_vanished_sources_with_a_notice() {
    let mut chat = ready_chat(3);
    for n in 0..3 {
        chat.toggle_remote_quote_source(0, n).expect("quote");
    }
    history_with(&mut chat, &[0, 2], Vec::new());
    let row = conversation(&mut chat);
    assert_eq!(row.quote_reply_contexts().len(), 2);
    assert!(row.take_quote_notice().is_some());
    assert!(row.take_quote_notice().is_none(), "the notice is one-shot");

    history_with(&mut chat, &[5], Vec::new());
    let row = conversation(&mut chat);
    assert!(!row.has_quote_reply(), "no sources left is a plain draft");
    assert!(row.take_quote_notice().is_some());
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Normal
    );
}

#[test]
fn a_pending_set_is_never_shrunk_and_restores_as_a_plain_draft() {
    let mut chat = ready_chat(3);
    conversation(&mut chat).set_composer_importance(RemoteChatMessageImportance::High);
    chat.toggle_remote_quote_source(0, 0).expect("first");
    chat.toggle_remote_quote_source(0, 1).expect("second");
    conversation(&mut chat).draft_mut().push_str("Pending");
    chat.queue_remote_message(0).expect("queues");
    history_with(&mut chat, &[0], Vec::new());
    let row = chat.remote_chats.get(0).expect("row");
    assert!(!row.has_quote_reply());
    assert_eq!(row.draft(), "Pending");
    assert_eq!(row.composer_importance(), RemoteChatMessageImportance::High);
    assert!(chat.take_actions().is_empty());
    assert!(conversation(&mut chat).take_quote_notice().is_some());
}

#[test]
fn reconciliation_requires_set_equality_ignoring_order() {
    let mut chat = ready_chat(3);
    let intent = uncertain_send(&mut chat, &[0, 1], 810);
    history_with(&mut chat, &[0, 1], vec![own_reply(&["src1", "src0"])]);
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
}

#[test]
fn a_subset_or_superset_of_the_sources_never_confirms() {
    for (reply, intent) in [
        (vec!["src0"], 820),
        (vec!["src0", "src1", "src2"], 821),
        (vec!["src0", "src2"], 822),
    ] {
        let mut chat = ready_chat(3);
        let sent = uncertain_send(&mut chat, &[0, 1], intent);
        history_with(&mut chat, &[0, 1, 2], vec![own_reply(&reply)]);
        assert_eq!(
            chat.complete_remote_send_reconciliation(sent),
            Some(RemoteReconciliationResolution::DraftRestored),
            "{reply:?}"
        );
        assert!(chat.remote_chats.get(0).expect("row").has_quote_reply());
    }
}

#[test]
fn plain_and_quote_replies_never_confirm_each_other() {
    let mut chat = ready_chat(2);
    let intent = uncertain_send(&mut chat, &[0, 1], 830);
    history_with(&mut chat, &[0, 1], vec![own_reply(&[])]);
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );

    let mut chat = ready_chat(2);
    conversation(&mut chat).draft_mut().push_str("Reply");
    chat.queue_remote_message(0).expect("plain send queues");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::SendRemoteMessage { submission, .. }] => *submission,
        _ => panic!("one plain action expected"),
    };
    let intent = WorkspaceIntentId::for_test(831);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    history_with(&mut chat, &[0, 1], vec![own_reply(&["src0"])]);
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
}

#[test]
fn a_non_normal_priority_row_never_confirms_a_quote_reply() {
    let mut chat = ready_chat(2);
    let intent = uncertain_send(&mut chat, &[0], 840);
    let high = RemoteChatMessage::for_test_with_quote_reference(
        "reply",
        "You",
        "2026-11-06T10:32:00Z",
        "Reply",
        true,
        RemoteChatMessageImportance::High,
        false,
        "src0",
        "Ada Lovelace",
        "p",
    );
    history_with(&mut chat, &[0, 1], vec![high]);
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
}

#[test]
fn history_rows_show_the_first_quote_and_a_more_count() {
    let mut chat = ready_chat(3);
    history_with(
        &mut chat,
        &[0, 1, 2],
        vec![own_reply(&["src0", "src1", "src2"])],
    );
    let row = chat.remote_chats.get(0).expect("row");
    let message = row.messages().last().expect("reply row");
    assert!(message.reply_context.is_some());
    assert_eq!(message.reply_context_extra, 2);
}
