use super::super::{ChatAction, ChatState, ConversationSelection, RemoteReconciliationResolution};
use super::{
    queue_remote_text, queue_remote_text_with_importance, remote_projection,
    remote_quote_source_history,
};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, RemoteChatProjection, WorkspaceIntentId,
};

#[test]
fn remote_text_send_stays_ephemeral_until_a_provider_projection_confirms_it() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote message",
    ));
    chat.select_remote_conversation(0);
    let message_count_before = chat.selected_messages().len();

    let submission = queue_remote_text(&mut chat, "  Send this remotely  ");

    assert!(
        chat.remote_chats
            .get(0)
            .is_some_and(|row| row.has_pending_send())
    );
    assert_eq!(chat.selected_messages().len(), message_count_before);
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());

    let intent = WorkspaceIntentId::for_test(1);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivered(intent));
    assert!(
        !chat
            .remote_chats
            .get(0)
            .is_some_and(|row| row.has_pending_send())
    );
    assert_eq!(chat.selected_messages().len(), message_count_before);
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_priority_metadata_stays_session_only_and_exposes_nondefault_badges() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    let _ = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:remote-chat@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Priority review",
            "AL",
            false,
            vec![
                RemoteChatMessage::for_test_with_importance(
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Please review this today",
                    false,
                    RemoteChatMessageImportance::High,
                    false,
                ),
                RemoteChatMessage::for_test_with_importance(
                    "Ada Lovelace",
                    "2026-11-06T10:31:00Z",
                    "Please respond immediately",
                    false,
                    RemoteChatMessageImportance::Urgent,
                    false,
                ),
            ],
        ),
    ]));

    assert_eq!(
        chat.remote_chats.message_importance(0, 0),
        Some(RemoteChatMessageImportance::High)
    );
    assert_eq!(
        chat.remote_chats.message_importance(0, 1),
        Some(RemoteChatMessageImportance::Urgent)
    );
    assert_eq!(
        RemoteChatMessageImportance::High.badge_label(),
        Some("High")
    );
    assert_eq!(
        RemoteChatMessageImportance::Urgent.badge_label(),
        Some("Urgent")
    );
    assert_eq!(RemoteChatMessageImportance::Normal.badge_label(), None);
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_pending_send_follows_its_target_across_refresh_and_retry_lifecycle() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Initial remote content",
    ));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut chat,
        "Retry this remote text",
        RemoteChatMessageImportance::High,
    );

    // The projection is replaced and may reorder, but its opaque target is
    // stable, so the pending draft must remain bound to this chat.
    chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Fresh remote content",
    ));
    assert_eq!(chat.selected_messages()[0].text, "Fresh remote content");
    assert!(
        chat.remote_chats
            .get(0)
            .is_some_and(|row| row.has_pending_send())
    );

    let intent = WorkspaceIntentId::for_test(2);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_retry_scheduled(intent));
    assert!(
        chat.remote_chats
            .get(0)
            .is_some_and(|row| row.has_pending_send())
    );

    assert!(chat.remote_send_failed(intent));
    let row = chat.remote_chats.get(0).expect("remote row must remain");
    assert!(!row.has_pending_send());
    assert_eq!(row.draft(), "Retry this remote text");
    assert_eq!(row.composer_importance(), RemoteChatMessageImportance::High);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_quote_reply_is_selected_history_only_normal_and_cancellable() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_projection(
        "19:quote-chat@thread.v2",
        "Directory preview must remain read-only",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            "19:quote-chat@thread.v2",
            "quote-source",
            "Please review this design.",
        ))
    );

    let row = chat
        .remote_chats
        .get_mut(0)
        .expect("selected row must exist");
    row.set_composer_importance(RemoteChatMessageImportance::High);
    chat.toggle_remote_quote_source(0, 0)
        .expect("a current target-bound remote row can enter quote mode");
    let row = chat.remote_chats.get(0).expect("selected row must remain");
    let quote = *row
        .quote_reply_contexts()
        .first()
        .expect("quote mode must retain safe source context");
    assert_eq!(quote.name, "Ada Lovelace");
    assert_eq!(quote.time, "Today 10:31");
    assert_eq!(quote.text, "Please review this design.");
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Normal
    );

    assert!(
        chat.remote_chats
            .get_mut(0)
            .expect("selected row must remain")
            .cancel_quote_reply()
    );
    let row = chat.remote_chats.get(0).expect("selected row must remain");
    assert!(row.quote_reply_contexts().is_empty());
    assert_eq!(row.composer_importance(), RemoteChatMessageImportance::High);

    chat.toggle_remote_quote_source(0, 0)
        .expect("the same current row may be selected again");
    chat.remote_chats
        .get_mut(0)
        .expect("selected row must remain")
        .draft_mut()
        .push_str("I agree with this approach.");
    chat.queue_remote_message(0)
        .expect("quote reply body must queue");
    let actions = chat.take_actions();
    assert_eq!(actions.len(), 1);
    match actions.into_iter().next().expect("one quote action") {
        ChatAction::ReplyToRemoteChatMessageWithQuote {
            submission: _,
            chat: target,
            messages,
            body,
        } => {
            assert_eq!(body, "I agree with this approach.");
            assert!(!format!("{target:?}{messages:?}").contains("quote-source"));
        }
        _ => panic!("selected remote quote must not become a generic/local action"),
    }
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn uncertain_remote_quote_reply_requires_the_exact_message_reference_target() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:quote-reconcile@thread.v2",
        "Directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            "19:quote-reconcile@thread.v2",
            "quoted-source",
            "Original source",
        ))
    );
    chat.toggle_remote_quote_source(0, 0)
        .expect("current source must be quoteable");
    chat.remote_chats
        .get_mut(0)
        .expect("selected row must remain")
        .draft_mut()
        .push_str("Same reply body");
    chat.queue_remote_message(0)
        .expect("quote reply must queue");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::ReplyToRemoteChatMessageWithQuote { submission, .. }] => *submission,
        _ => panic!("quote test must produce exactly one quote action"),
    };
    let intent = WorkspaceIntentId::for_test(710);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));

    // Matching own body + Normal priority is still insufficient when the
    // Graph messageReference points at a different opaque source target.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:quote-reconcile@thread.v2",
            vec![
                RemoteChatMessage::for_test_with_reactions(
                    "quoted-source",
                    "Ada Lovelace",
                    "2026-11-06T10:31:00Z",
                    "Original source",
                    false,
                    false,
                    Vec::new(),
                ),
                RemoteChatMessage::for_test_with_quote_reference(
                    "reply-message",
                    "You",
                    "2026-11-06T10:32:00Z",
                    "Same reply body",
                    true,
                    RemoteChatMessageImportance::Normal,
                    false,
                    "different-source",
                    "Ada Lovelace",
                    "Other source",
                ),
            ],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
    let row = chat.remote_chats.get(0).expect("selected row must remain");
    assert_eq!(row.draft(), "Same reply body");
    assert!(!row.quote_reply_contexts().is_empty());

    // A fresh uncertain attempt may be confirmed only when the current
    // selected history reports the exact source target in messageReference.
    chat.queue_remote_message(0)
        .expect("restored quote reply may be retried explicitly");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::ReplyToRemoteChatMessageWithQuote { submission, .. }] => *submission,
        _ => panic!("retry must remain a quote action"),
    };
    let retry_intent = WorkspaceIntentId::for_test(711);
    assert!(chat.bind_remote_submission(submission, retry_intent));
    assert!(chat.remote_send_delivery_uncertain(retry_intent));
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:quote-reconcile@thread.v2",
            vec![RemoteChatMessage::for_test_with_quote_reference(
                "reply-message",
                "You",
                "2026-11-06T10:33:00Z",
                "Same reply body",
                true,
                RemoteChatMessageImportance::Normal,
                false,
                "quoted-source",
                "Ada Lovelace",
                "Original source",
            )],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(retry_intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
}

#[test]
fn rejected_quote_reply_with_a_deleted_source_restores_an_ordinary_unlocked_draft() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:quote-deleted@thread.v2",
        "Directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            "19:quote-deleted@thread.v2",
            "deleted-source",
            "Soon deleted source",
        ))
    );
    chat.remote_chats
        .get_mut(0)
        .expect("selected row must remain")
        .set_composer_importance(RemoteChatMessageImportance::Urgent);
    chat.toggle_remote_quote_source(0, 0)
        .expect("current source must be quoteable");
    chat.remote_chats
        .get_mut(0)
        .expect("selected row must remain")
        .draft_mut()
        .push_str("Retain this body");
    chat.queue_remote_message(0)
        .expect("quote reply must queue");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::ReplyToRemoteChatMessageWithQuote { submission, .. }] => *submission,
        _ => panic!("quote test must produce exactly one quote action"),
    };
    let intent = WorkspaceIntentId::for_test(712);
    assert!(chat.bind_remote_submission(submission, intent));

    // The request is already bound, so its terminal outcome still routes,
    // but a source removed from current history cannot be restored as an
    // unstageable quote selection.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:quote-deleted@thread.v2",
            Vec::new()
        ))
    );
    assert!(chat.remote_send_failed(intent));
    let row = chat.remote_chats.get(0).expect("selected row must remain");
    assert!(!row.has_pending_send());
    assert!(row.quote_reply_contexts().is_empty());
    assert_eq!(row.draft(), "Retain this body");
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Urgent
    );
    chat.queue_remote_message(0)
        .expect("restored ordinary draft must not remain locked");
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SendRemoteMessage {
            importance: RemoteChatMessageImportance::Urgent,
            ..
        }]
    ));
}

#[test]
fn remote_delivery_uncertainty_locks_the_draft_until_a_fresh_projection_confirms_it() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    let _ = chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote content",
    ));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut chat,
        "Check whether this delivered",
        RemoteChatMessageImportance::Urgent,
    );
    let intent = WorkspaceIntentId::for_test(3);
    assert!(chat.bind_remote_submission(submission, intent));

    // Network and 5xx outcomes are ambiguous for POST. Keep the exact body
    // locked until the controller's specifically scheduled exact selected history can
    // prove whether it appeared; a second action must not be emitted first.
    assert!(chat.remote_send_delivery_uncertain(intent));
    let row = chat.remote_chats.get(0).expect("remote row must remain");
    assert!(row.has_pending_send());
    assert!(row.is_awaiting_reconciliation());
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Urgent
    );
    assert!(matches!(
        chat.queue_remote_message(0),
        Err(super::super::remote::RemoteSendError::Pending)
    ));
    assert!(chat.take_actions().is_empty());

    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:remote-chat@thread.v2",
            vec![RemoteChatMessage::for_test_with_importance(
                "You",
                "2026-11-06T10:31:00Z",
                "Check whether this delivered",
                true,
                RemoteChatMessageImportance::Urgent,
                false,
            )],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
    let row = chat.remote_chats.get(0).expect("remote row must remain");
    assert!(!row.has_pending_send());
    assert!(row.draft().is_empty());
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Normal
    );
    assert!(chat.take_actions().is_empty());
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn directory_preview_never_confirms_an_ambiguous_remote_send() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote content",
    ));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut chat,
        "Directory cannot prove this",
        RemoteChatMessageImportance::Normal,
    );
    let intent = WorkspaceIntentId::for_test(30);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));

    let _ = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:remote-chat@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            false,
            vec![RemoteChatMessage::for_test_with_importance(
                "You",
                "2026-11-06T10:31:00Z",
                "Directory cannot prove this",
                true,
                RemoteChatMessageImportance::Normal,
                false,
            )],
        ),
    ]));
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
}

#[test]
fn urgent_send_is_confirmed_by_history_after_the_user_navigated_away() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:remote-chat@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            false,
            Vec::new(),
        ),
        RemoteChatConversation::for_test(
            "19:other@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Compilers",
            "GH",
            false,
            Vec::new(),
        ),
    ]));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut chat,
        "Sent before I left",
        RemoteChatMessageImportance::Urgent,
    );
    let intent = WorkspaceIntentId::for_test(31);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    chat.select_remote_conversation(1);

    let rows_before = chat.remote_chats.get(0).expect("row").messages().len();
    assert!(
        !chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:remote-chat@thread.v2",
            vec![RemoteChatMessage::for_test_with_importance(
                "You",
                "2026-11-06T10:31:00Z",
                "Sent before I left",
                true,
                RemoteChatMessageImportance::Urgent,
                false,
            )],
        ))
    );
    assert_eq!(
        chat.remote_chats.get(0).expect("row").messages().len(),
        rows_before,
        "an unselected chat's rows are never replaced by observation"
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
}

#[test]
fn remote_delivery_uncertainty_restores_priority_when_reconciliation_lacks_an_exact_match() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote content",
    ));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut chat,
        "Check whether this delivered",
        RemoteChatMessageImportance::High,
    );
    let intent = WorkspaceIntentId::for_test(4);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));

    // The same own body with Normal priority is not an acknowledgement of
    // the ambiguous High-priority POST. A reconciliation match must include
    // the typed priority as well as the text.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:remote-chat@thread.v2",
            vec![RemoteChatMessage::for_test_with_importance(
                "You",
                "2026-11-06T10:31:00Z",
                "Check whether this delivered",
                true,
                RemoteChatMessageImportance::Normal,
                false,
            )],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
    let row = chat.remote_chats.get(0).expect("remote row must remain");
    assert_eq!(row.draft(), "Check whether this delivered");
    assert_eq!(row.composer_importance(), RemoteChatMessageImportance::High);
    chat.queue_remote_message(0)
        .expect("the user may explicitly retry after reconciliation");
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SendRemoteMessage {
            importance: RemoteChatMessageImportance::High,
            ..
        }]
    ));

    // If Graph itself cannot provide the reconciliation snapshot, retain the
    // draft as well rather than leaving the composer locked forever.
    let mut failed = ChatState::default();
    let _ = failed.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote content",
    ));
    failed.select_remote_conversation(0);
    let submission = queue_remote_text_with_importance(
        &mut failed,
        "Restore after sync failure",
        RemoteChatMessageImportance::Urgent,
    );
    let failed_intent = WorkspaceIntentId::for_test(5);
    assert!(failed.bind_remote_submission(submission, failed_intent));
    assert!(failed.remote_send_delivery_uncertain(failed_intent));
    assert!(failed.fail_remote_send_reconciliation(failed_intent));
    let row = failed.remote_chats.get(0).expect("remote row must remain");
    assert_eq!(row.draft(), "Restore after sync failure");
    assert_eq!(
        row.composer_importance(),
        RemoteChatMessageImportance::Urgent
    );
}

#[test]
fn absent_projection_cancels_an_unstarted_bound_send_but_retains_its_draft() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:visible@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            false,
            vec![],
        ),
        RemoteChatConversation::for_test(
            "19:missing@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Planning",
            "GH",
            false,
            vec![],
        ),
    ]));
    let missing_conversation = chat
        .remote_chats
        .get_mut(1)
        .expect("second remote chat must exist");
    missing_conversation.set_composer_importance(RemoteChatMessageImportance::Urgent);
    missing_conversation
        .draft_mut()
        .push_str("Do not send after this row disappears");
    chat.queue_remote_message(1)
        .expect("remote message must queue");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::SendRemoteMessage { submission, .. }] => *submission,
        _ => panic!("the hidden chat test must create exactly one remote action"),
    };
    let intent = WorkspaceIntentId::for_test(6);
    assert!(chat.bind_remote_submission(submission, intent));

    let cancel_unstarted = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:visible@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            false,
            vec![],
        ),
    ]));
    assert_eq!(cancel_unstarted, [intent]);
    assert!(chat.remote_chats.get(1).is_none());
    assert!(chat.remote_send_failed(intent));

    let _ = chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:missing@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Planning",
            "GH",
            false,
            vec![],
        ),
    ]));
    let restored = chat
        .remote_chats
        .get(0)
        .expect("the hidden target must restore when Graph returns it");
    assert_eq!(restored.draft(), "Do not send after this row disappears");
    assert_eq!(
        restored.composer_importance(),
        RemoteChatMessageImportance::Urgent
    );
}

#[test]
fn clearing_remote_projection_drops_a_pending_remote_action() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    let _ = chat.apply_remote_projection(remote_projection(
        "19:remote-chat@thread.v2",
        "Existing remote content",
    ));
    chat.select_remote_conversation(0);

    // If an account switch clears the projection before the UI has drained a
    // just-created feature action, that action must not target the old chat.
    chat.remote_chats
        .get_mut(0)
        .expect("remote row must remain")
        .draft_mut()
        .push_str("Discard this on disconnect");
    chat.queue_remote_message(0)
        .expect("another remote message must queue");
    assert_eq!(chat.pending_actions.len(), 1);

    chat.clear_remote_projection();
    assert!(chat.remote_chats.is_empty());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert!(chat.take_actions().is_empty());
    assert!(!chat.remote_send_failed(WorkspaceIntentId::for_test(3)));
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn desynced_or_remote_selection_has_no_local_state_and_does_not_panic() {
    let mut chat = ChatState::default();
    for selection in [
        ConversationSelection::Favorite(usize::MAX),
        ConversationSelection::Workspace(usize::MAX),
        ConversationSelection::Local(usize::MAX),
        ConversationSelection::Remote(usize::MAX),
    ] {
        chat.selected = selection;
        assert!(chat.selected_composer_mut().is_none());
        assert!(chat.selected_notes_mut().is_none());
        assert!(chat.messages_for_mut(selection).is_none());
        assert!(chat.selected_messages().is_empty());
        let _ = chat.send();
    }
}

#[test]
fn rich_send_reconciles_on_plain_projection() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection("19:remote-chat@thread.v2", "Existing"));
    chat.select_remote_conversation(0);
    let intent = WorkspaceIntentId::for_test(21);
    let submission = queue_remote_text(&mut chat, "Ship **it   now** please");
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    // Graph returns HTML, so layout collapses before the projection is read.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:remote-chat@thread.v2",
            vec![RemoteChatMessage::for_test(
                "You",
                "2026-11-06T10:31:00Z",
                "Ship it now please",
                true,
                false,
            )],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
}

#[test]
fn plain_reconciliation_stays_exact() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection("19:remote-chat@thread.v2", "Existing"));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text(&mut chat, "two  spaces");
    let intent = WorkspaceIntentId::for_test(22);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            "19:remote-chat@thread.v2",
            vec![RemoteChatMessage::for_test(
                "You",
                "2026-11-06T10:31:00Z",
                "two spaces",
                true,
                false,
            )],
        ))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
}

#[test]
fn a_rejected_rich_send_restores_the_users_own_markup() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection("19:remote-chat@thread.v2", "Existing"));
    chat.select_remote_conversation(0);
    let submission = queue_remote_text(&mut chat, "see [docs](https://example.com) **now**");
    let intent = WorkspaceIntentId::for_test(23);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_failed(intent));
    let row = chat.remote_chats.get(0).expect("row");
    assert_eq!(row.draft(), "see [docs](https://example.com) **now**");
    assert!(!row.has_pending_send());
}

#[test]
fn a_quote_reply_body_is_never_interpreted_as_markup() {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection("19:quote-chat@thread.v2", "Existing"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            "19:quote-chat@thread.v2",
            "quote-source",
            "Please review this design.",
        ))
    );
    chat.toggle_remote_quote_source(0, 0).expect("quote mode");
    chat.remote_chats
        .get_mut(0)
        .expect("row")
        .draft_mut()
        .push_str("keep **these** stars");
    chat.queue_remote_message(0).expect("quote reply queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::ReplyToRemoteChatMessageWithQuote { body, .. } => {
            assert_eq!(body, "keep **these** stars");
        }
        _ => panic!("expected a quote reply"),
    }
}
