use super::super::remote::RemoteSendError;
use super::super::{
    ChatAction, ChatState, ConversationSelection, MessageAction, RemoteMutationLabel,
};
use super::remote_projection;
use crate::integrations::{
    Capability, ProviderResourceRef, RemoteCapabilities, RemoteChatMessage,
    RemoteChatMessagesProjection, WorkspaceIntentId,
};
use crate::ui::toast::ToastState;

const CHAT: &str = "19:edit-chat@thread.v2";
const OTHER_CHAT: &str = "19:other-chat@thread.v2";

fn own(id: &str, text: &str) -> RemoteChatMessage {
    RemoteChatMessage::for_test_own(id, text, true, false)
}

fn history(chat: &str, rows: Vec<RemoteChatMessage>) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(chat, rows)
}

/// A selected remote chat whose exact history is Ready with `rows`.
fn ready_chat(rows: Vec<RemoteChatMessage>) -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT, "preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(chat.apply_remote_chat_messages_projection(history(CHAT, rows)));
    chat
}

fn single_own_row() -> ChatState {
    ready_chat(vec![own("m1", "hello world")])
}

fn caps(edit: Capability, delete: Capability) -> RemoteCapabilities {
    RemoteCapabilities {
        chat_message_edit: edit,
        chat_message_delete: delete,
        member_presence: Capability::Unknown,
        chat_mark_read: Capability::Unknown,
        file_attach: Capability::Unknown,
    }
}

fn view(chat: &ChatState) -> Option<(bool, bool, bool)> {
    chat.remote_chats
        .message_actions_view(0, 0, chat.effective_remote_capabilities())
        .map(|view| (view.show_edit, view.edit_enabled, view.show_delete))
}

fn edit_to(
    chat: &mut ChatState,
    text: &str,
) -> (super::super::RemoteSubmissionId, ProviderResourceRef) {
    chat.begin_remote_edit(0, 0).expect("edit mode must begin");
    *chat.remote_chats.get_mut(0).expect("chat").draft_mut() = text.into();
    chat.queue_remote_message(0).expect("edit must queue");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::EditRemoteChatMessage {
            submission,
            message,
            body,
            ..
        } => {
            assert_eq!(
                body,
                crate::integrations::ComposedBody::from_markdown_lite(text.trim())
            );
            (submission, message)
        }
        other => panic!("expected an edit action, got {other:?}"),
    }
}

fn delete_message(chat: &mut ChatState) -> super::super::RemoteSubmissionId {
    chat.request_remote_delete(0, 0)
        .expect("confirmation opens");
    assert!(chat.take_actions().is_empty(), "no write before confirming");
    chat.confirm_remote_delete()
        .expect("confirmed delete queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::DeleteRemoteChatMessage { submission, .. } => submission,
        other => panic!("expected a delete action, got {other:?}"),
    }
}

fn bind(
    chat: &mut ChatState,
    submission: super::super::RemoteSubmissionId,
    n: u64,
) -> WorkspaceIntentId {
    let intent = WorkspaceIntentId::for_test(n);
    assert!(chat.bind_remote_submission(submission, intent));
    intent
}

fn draft(chat: &ChatState) -> String {
    chat.remote_chats.get(0).expect("chat").draft().to_owned()
}

fn locked(chat: &ChatState) -> bool {
    chat.remote_chats.get(0).expect("chat").has_pending_send()
}

#[test]
fn affordances_are_gated_by_history_ownership_target_capability_and_gates() {
    let chat = single_own_row();
    assert_eq!(view(&chat), Some((true, true, true)));

    // A row that is not the user's own, is deleted, or has no target.
    let other = RemoteChatMessage::for_test_with_id_and_current_user_mention(
        "m2", "Ada", "now", "not mine", false,
    );
    let deleted = RemoteChatMessage::for_test_own("m3", "gone", true, true);
    let anonymous = RemoteChatMessage::for_test("Me", "now", "no id", true, false);
    let chat = ready_chat(vec![other, deleted, anonymous]);
    for index in 0..3 {
        assert!(
            chat.remote_chats
                .message_actions_view(0, index, chat.effective_remote_capabilities())
                .is_none(),
            "row {index}"
        );
    }

    // Not plain-text editable: delete only.
    let chat = ready_chat(vec![RemoteChatMessage::for_test_own(
        "m1", "rich", false, false,
    )]);
    assert_eq!(view(&chat), Some((false, false, true)));

    // Capabilities.
    for (edit, delete, expected) in [
        (
            Capability::NotGranted,
            Capability::Available,
            Some((false, false, true)),
        ),
        (
            Capability::UnsupportedInCloud,
            Capability::Available,
            Some((false, false, true)),
        ),
        (
            Capability::Available,
            Capability::NotGranted,
            Some((true, true, false)),
        ),
        (Capability::NotGranted, Capability::NotGranted, None),
        (
            Capability::Unknown,
            Capability::Unknown,
            Some((true, true, true)),
        ),
    ] {
        let mut chat = single_own_row();
        chat.apply_remote_capabilities(caps(edit, delete));
        assert_eq!(view(&chat), expected, "{edit:?} {delete:?}");
    }

    // History not Ready: a directory refresh returns the chat to a preview.
    let mut chat = single_own_row();
    chat.apply_remote_projection(remote_projection(CHAT, "refreshed preview"));
    assert_eq!(view(&chat), None);

    // A draft keeps Edit visible but disabled so it cannot be overwritten.
    let mut chat = single_own_row();
    chat.remote_chats
        .get_mut(0)
        .expect("chat")
        .draft_mut()
        .push_str("unsent");
    assert_eq!(view(&chat), Some((true, false, true)));
    assert_eq!(
        chat.begin_remote_edit(0, 0),
        Err(RemoteSendError::EditNeedsEmptyDraft)
    );
}

#[test]
fn unchanged_edit_body_is_rejected_locally_and_sends_nothing() {
    let mut chat = single_own_row();
    chat.begin_remote_edit(0, 0).expect("edit mode");
    assert_eq!(draft(&chat), "hello world");
    assert_eq!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::Unchanged)
    );
    // Whitespace-only differences are unchanged too.
    *chat.remote_chats.get_mut(0).expect("chat").draft_mut() = "  hello   world \n".into();
    assert_eq!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::Unchanged)
    );
    assert!(chat.take_actions().is_empty());
    assert!(chat.remote_chats.get(0).expect("chat").is_editing());
    *chat.remote_chats.get_mut(0).expect("chat").draft_mut() = "   ".into();
    assert_eq!(chat.queue_remote_message(0), Err(RemoteSendError::Empty));
}

#[test]
fn cancelling_an_edit_restores_an_empty_composer() {
    let mut chat = single_own_row();
    chat.begin_remote_edit(0, 0).expect("edit mode");
    assert!(chat.remote_chats.get_mut(0).expect("chat").cancel_edit());
    assert_eq!(draft(&chat), "");
    assert!(!chat.remote_chats.get(0).expect("chat").is_editing());
    assert_eq!(view(&chat), Some((true, true, true)));
}

#[test]
fn a_delivered_edit_has_no_optimistic_row_and_unlocks_on_the_next_exact_history() {
    let mut chat = single_own_row();
    let stored_before = chat.stored();
    let (submission, _) = edit_to(&mut chat, "  hello there  ");
    let intent = bind(&mut chat, submission, 11);

    assert!(locked(&chat));
    assert_eq!(draft(&chat), "");
    assert_eq!(chat.selected_messages()[0].text, "hello world");

    assert!(chat.remote_message_mutation_delivered(intent));
    // 204 is delivery, but the old row is still shown: stay locked.
    assert!(locked(&chat));
    assert_eq!(chat.selected_messages()[0].text, "hello world");
    assert_eq!(view(&chat), None);

    assert!(
        chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "hello there")]))
    );
    assert!(!locked(&chat));
    assert_eq!(chat.selected_messages()[0].text, "hello there");
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn a_failed_refresh_after_delivery_still_releases_the_gate() {
    let mut chat = single_own_row();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 12);
    assert!(chat.remote_message_mutation_delivered(intent));
    assert!(locked(&chat));
    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::unavailable_for_test(CHAT)
    ));
    assert!(!locked(&chat));
}

#[test]
fn edit_confirmation_requires_an_own_undeleted_row_with_the_requested_text() {
    // Applied: the row now reads the new text (layout differences ignored).
    let mut chat = single_own_row();
    let (submission, _) = edit_to(&mut chat, "line one\nline two");
    let intent = bind(&mut chat, submission, 21);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    assert!(locked(&chat));
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "line one line two")]));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert_eq!(resolution.label, RemoteMutationLabel::Edit);
    assert!(resolution.confirmed);
    assert!(!locked(&chat));
    assert_eq!(draft(&chat), "");

    // Not applied: the row still has the old text. Back into edit mode.
    let mut chat = single_own_row();
    let (submission, _) = edit_to(&mut chat, "hello there");
    let intent = bind(&mut chat, submission, 22);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "hello world")]));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(!resolution.confirmed);
    assert!(chat.remote_chats.get(0).expect("chat").is_editing());
    assert_eq!(draft(&chat), "hello there");
    assert!(!locked(&chat));

    // The row disappeared: a missing row is never proof; plain draft.
    let mut chat = single_own_row();
    let (submission, _) = edit_to(&mut chat, "hello there");
    let intent = bind(&mut chat, submission, 23);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m9", "other")]));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(!resolution.confirmed);
    assert!(!chat.remote_chats.get(0).expect("chat").is_editing());
    assert_eq!(draft(&chat), "hello there");

    // A matching row that is deleted, or not the user's, confirms nothing.
    for row in [
        RemoteChatMessage::for_test_own("m1", "hello there", true, true),
        RemoteChatMessage::for_test_with_id_and_current_user_mention(
            "m1",
            "Ada",
            "now",
            "hello there",
            false,
        ),
    ] {
        let mut chat = single_own_row();
        let (submission, _) = edit_to(&mut chat, "hello there");
        let intent = bind(&mut chat, submission, 24);
        assert!(chat.remote_message_mutation_delivery_uncertain(intent));
        chat.apply_remote_chat_messages_projection(history(CHAT, vec![row]));
        let resolution = chat
            .complete_remote_message_mutation_reconciliation(intent)
            .expect("settled");
        assert!(!resolution.confirmed);
    }
}

#[test]
fn delete_confirmation_requires_the_exact_row_marked_deleted() {
    let mut chat = single_own_row();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 31);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    chat.apply_remote_chat_messages_projection(history(
        CHAT,
        vec![RemoteChatMessage::for_test_own("m1", "x", true, true)],
    ));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert_eq!(resolution.label, RemoteMutationLabel::Delete);
    assert!(resolution.confirmed);

    // Row still live: unconfirmed, unlocked, controls come back.
    let mut chat = single_own_row();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 32);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "hello world")]));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(!resolution.confirmed);
    assert!(!locked(&chat));
    assert_eq!(view(&chat), Some((true, true, true)));

    // A missing row is never proof of deletion.
    let mut chat = single_own_row();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 33);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    chat.apply_remote_chat_messages_projection(history(CHAT, Vec::new()));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(!resolution.confirmed);
}

#[test]
fn another_chats_history_never_confirms_a_mutation() {
    let mut chat = single_own_row();
    chat.apply_remote_projection(super::remote_projection(CHAT, "preview"));
    // Keep the exact history Ready after the directory refresh.
    let _ = chat.take_remote_chat_messages_request();
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "hello world")]));
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 41);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));

    // A deleted row for the same message ID, but from a different chat.
    assert!(!chat.apply_remote_chat_messages_projection(history(
        OTHER_CHAT,
        vec![RemoteChatMessage::for_test_own("m1", "x", true, true)],
    )));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(!resolution.confirmed);
}

#[test]
fn a_confirming_history_after_navigation_still_settles_the_mutation() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT, "preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m1", "hello world")]));
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 42);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));

    chat.selected = ConversationSelection::Workspace(0);
    assert!(!chat.apply_remote_chat_messages_projection(history(
        CHAT,
        vec![RemoteChatMessage::for_test_own("m1", "x", true, true)],
    )));
    let resolution = chat
        .complete_remote_message_mutation_reconciliation(intent)
        .expect("settled");
    assert!(resolution.confirmed);
}

#[test]
fn a_known_rejection_unlocks_the_exact_gate_and_restores_the_edit() {
    let mut chat = single_own_row();
    let (submission, _) = edit_to(&mut chat, "hello there");
    let intent = bind(&mut chat, submission, 51);
    // An unrelated intent changes nothing.
    assert!(
        chat.remote_message_mutation_rejected(WorkspaceIntentId::for_test(999), false)
            .is_none()
    );
    assert!(locked(&chat));
    assert_eq!(
        chat.remote_message_mutation_rejected(intent, false),
        Some(RemoteMutationLabel::Edit)
    );
    assert!(!locked(&chat));
    assert!(chat.remote_chats.get(0).expect("chat").is_editing());
    assert_eq!(draft(&chat), "hello there");
    // The capability is untouched by an ordinary rejection.
    assert_eq!(
        chat.effective_remote_capabilities().chat_message_edit,
        Capability::Unknown
    );
}

#[test]
fn a_403_marks_only_that_action_not_granted_for_the_session_and_unlocks() {
    let mut chat = single_own_row();
    chat.apply_remote_capabilities(caps(Capability::Unknown, Capability::Unknown));
    let (submission, _) = edit_to(&mut chat, "hello there");
    let intent = bind(&mut chat, submission, 61);
    assert_eq!(
        chat.remote_message_mutation_rejected(intent, true),
        Some(RemoteMutationLabel::Edit)
    );
    assert!(!locked(&chat));
    let effective = chat.effective_remote_capabilities();
    assert_eq!(effective.chat_message_edit, Capability::NotGranted);
    assert_eq!(effective.chat_message_delete, Capability::Unknown);

    // A later identity poll cannot resurrect the denied action.
    chat.apply_remote_capabilities(caps(Capability::Available, Capability::Available));
    assert_eq!(
        chat.effective_remote_capabilities().chat_message_edit,
        Capability::NotGranted
    );
    // The rejected edit is back in edit mode; leaving it shows the controls.
    assert!(chat.remote_chats.get_mut(0).expect("chat").cancel_edit());
    let remaining = view(&chat).expect("delete remains");
    assert!(!remaining.0, "edit stays hidden");
    assert!(remaining.2, "delete stays offered");

    // And the same for deletion.
    let mut chat = single_own_row();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 62);
    assert_eq!(
        chat.remote_message_mutation_rejected(intent, true),
        Some(RemoteMutationLabel::Delete)
    );
    assert_eq!(
        chat.effective_remote_capabilities().chat_message_delete,
        Capability::NotGranted
    );
    assert_eq!(view(&chat), Some((true, true, false)));

    // The denial is session state: dropping the remote projection resets it.
    chat.clear_remote_projection();
    assert_eq!(
        chat.effective_remote_capabilities(),
        RemoteCapabilities::default()
    );
}

#[test]
fn failed_reconciliation_unlocks_and_restores_like_an_unconfirmed_outcome() {
    let mut chat = single_own_row();
    let (submission, _) = edit_to(&mut chat, "hello there");
    let intent = bind(&mut chat, submission, 71);
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    assert_eq!(
        chat.fail_remote_message_mutation_reconciliation(intent),
        Some(RemoteMutationLabel::Edit)
    );
    assert!(!locked(&chat));
    assert!(chat.remote_chats.get(0).expect("chat").is_editing());
    assert_eq!(draft(&chat), "hello there");
}

#[test]
fn mutations_are_mutually_exclusive_with_sends_quotes_edits_and_reactions() {
    // A pending send blocks both actions.
    let mut chat = single_own_row();
    chat.remote_chats
        .get_mut(0)
        .expect("chat")
        .draft_mut()
        .push_str("outgoing");
    chat.queue_remote_message(0).expect("send queues");
    assert_eq!(view(&chat), None);
    assert_eq!(chat.begin_remote_edit(0, 0), Err(RemoteSendError::Pending));
    assert_eq!(
        chat.request_remote_delete(0, 0),
        Err(RemoteSendError::DeleteUnavailable)
    );

    // Quote mode blocks them, and edit mode blocks quoting and deleting.
    let mut chat = ready_chat(vec![own("m1", "hello world"), own("m2", "second")]);
    chat.toggle_remote_quote_source(0, 1).expect("quote mode");
    assert_eq!(
        chat.begin_remote_edit(0, 0),
        Err(RemoteSendError::EditBlockedByQuote)
    );
    let mut chat = ready_chat(vec![own("m1", "hello world"), own("m2", "second")]);
    chat.begin_remote_edit(0, 0).expect("edit mode");
    assert_eq!(
        chat.toggle_remote_quote_source(0, 1),
        Err(RemoteSendError::QuoteBlockedByEdit)
    );
    assert!(
        !chat
            .remote_chats
            .quote_reply_view(0, 1)
            .expect("view")
            .can_quote
    );
    assert_eq!(chat.begin_remote_edit(0, 1), Err(RemoteSendError::Pending));
    assert_eq!(
        chat.request_remote_delete(0, 1),
        Err(RemoteSendError::DeleteUnavailable)
    );

    // A pending mutation blocks sending, quoting and reacting on its message.
    let mut chat = ready_chat(vec![own("m1", "hello world")]);
    let submission = delete_message(&mut chat);
    let _ = bind(&mut chat, submission, 81);
    chat.remote_chats
        .get_mut(0)
        .expect("chat")
        .draft_mut()
        .push_str("outgoing");
    assert_eq!(chat.queue_remote_message(0), Err(RemoteSendError::Pending));
    assert!(chat.take_actions().is_empty());
    let reaction = chat
        .remote_chats
        .reaction_view(0, 0, crate::shared::reaction::ReactionKind::Like)
        .expect("view");
    assert!(!reaction.can_react);
    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(crate::shared::reaction::ReactionKind::Like),
        &mut toast,
    );
    assert!(chat.take_actions().is_empty());

    // A pending reaction on a message hides its edit and delete controls.
    let mut chat = ready_chat(vec![own("m1", "hello world")]);
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(crate::shared::reaction::ReactionKind::Like),
        &mut toast,
    );
    assert_eq!(chat.take_actions().len(), 1);
    assert_eq!(view(&chat), None);
}

#[test]
fn delete_needs_confirmation_and_rechecks_the_exact_row() {
    let mut chat = single_own_row();
    chat.request_remote_delete(0, 0)
        .expect("confirmation opens");
    assert!(chat.take_actions().is_empty());
    // Cancelling (dismissing) leaves nothing queued.
    chat.dismiss_contextual_overlays();
    assert_eq!(
        chat.confirm_remote_delete(),
        Err(RemoteSendError::DeleteUnavailable)
    );
    assert!(chat.take_actions().is_empty());

    // The row vanished before the user confirmed.
    let mut chat = single_own_row();
    chat.request_remote_delete(0, 0)
        .expect("confirmation opens");
    chat.apply_remote_chat_messages_projection(history(CHAT, vec![own("m9", "other")]));
    assert_eq!(
        chat.confirm_remote_delete(),
        Err(RemoteSendError::DeleteUnavailable)
    );
    assert!(chat.take_actions().is_empty());
    assert!(!locked(&chat));

    // The capability was denied meanwhile.
    let mut chat = single_own_row();
    chat.request_remote_delete(0, 0)
        .expect("confirmation opens");
    chat.apply_remote_capabilities(caps(Capability::Available, Capability::NotGranted));
    assert_eq!(
        chat.confirm_remote_delete(),
        Err(RemoteSendError::DeleteUnavailable)
    );
    assert!(chat.take_actions().is_empty());
}

#[test]
fn an_unbound_action_is_pruned_and_the_edit_text_is_restored() {
    // The action is queued but never bound before the row disappears.
    let mut chat2 = single_own_row();
    chat2.begin_remote_edit(0, 0).expect("edit mode");
    *chat2.remote_chats.get_mut(0).expect("chat").draft_mut() = "hello there".into();
    chat2.queue_remote_message(0).expect("edit queues");
    chat2.apply_remote_chat_messages_projection(history(CHAT, vec![own("m9", "other")]));
    assert!(
        chat2.take_actions().is_empty(),
        "a stale edit must not dispatch"
    );
    assert!(!locked(&chat2));
    assert_eq!(draft(&chat2), "hello there");
    assert!(!chat2.remote_chats.get(0).expect("chat").is_editing());
}

#[test]
fn a_bound_mutation_survives_a_directory_refresh_and_never_persists() {
    let mut chat = single_own_row();
    let stored_before = chat.stored();
    let submission = delete_message(&mut chat);
    let intent = bind(&mut chat, submission, 91);
    chat.apply_remote_projection(remote_projection(CHAT, "refreshed preview"));
    assert!(locked(&chat));
    assert!(chat.remote_message_mutation_owned(intent));
    assert!(chat.remote_message_mutation_delivery_uncertain(intent));
    assert!(!chat.is_persistence_dirty());
    assert_eq!(chat.stored(), stored_before);
}

fn text_run(value: &str) -> crate::integrations::Inline {
    crate::integrations::Inline::Text {
        text: value.into(),
        marks: crate::integrations::Marks::default(),
    }
}

fn bold_run(value: &str) -> crate::integrations::Inline {
    crate::integrations::Inline::Text {
        text: value.into(),
        marks: crate::integrations::Marks {
            bold: true,
            italic: false,
            code: false,
        },
    }
}

fn rich_own(id: &str, runs: Vec<crate::integrations::Inline>) -> RemoteChatMessage {
    let text: String = runs
        .iter()
        .map(|run| match run {
            crate::integrations::Inline::Text { text, .. }
            | crate::integrations::Inline::Link { text, .. } => text.as_str(),
            crate::integrations::Inline::Mention { display, .. } => display.as_str(),
        })
        .collect();
    RemoteChatMessage::for_test_own(id, text, true, false).with_rich_for_test(runs)
}

#[test]
fn rich_edit_roundtrip() {
    let mut chat = ready_chat(vec![rich_own(
        "m1",
        vec![text_run("see "), bold_run("this"), text_run("\nnow")],
    )]);
    chat.begin_remote_edit(0, 0)
        .expect("a rich row enters edit mode");
    assert_eq!(draft(&chat), "see **this**\nnow");
    // Unchanged as written.
    assert_eq!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::Unchanged)
    );
    *chat.remote_chats.get_mut(0).expect("chat").draft_mut() = "see **this** later\nnow".into();
    chat.queue_remote_message(0).expect("edit queues");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::EditRemoteChatMessage { body, .. } => {
            assert!(!body.is_plain());
            assert_eq!(body.plain_projection(), "see this later\nnow");
        }
        other => panic!("expected an edit, got {other:?}"),
    }
}

#[test]
fn formatting_only_change_is_not_unchanged() {
    let mut chat = ready_chat(vec![own("m1", "hello world")]);
    chat.begin_remote_edit(0, 0).expect("edit mode");
    *chat.remote_chats.get_mut(0).expect("chat").draft_mut() = "**hello** world".into();
    chat.queue_remote_message(0)
        .expect("same words, new formatting, is a real edit");
    match chat.take_actions().into_iter().next().expect("one action") {
        ChatAction::EditRemoteChatMessage { body, .. } => assert!(!body.is_plain()),
        other => panic!("expected an edit, got {other:?}"),
    }
}

#[test]
fn lossy_roundtrip_is_unavailable() {
    // Literal marker characters and padded bold cannot be written back.
    for runs in [
        vec![bold_run("x"), text_run(" and 2 ** 3 ** 4 a**b**")],
        vec![bold_run(" padded "), text_run("end")],
    ] {
        let mut chat = ready_chat(vec![rich_own("m1", runs)]);
        assert_eq!(
            chat.begin_remote_edit(0, 0),
            Err(RemoteSendError::EditFormatting)
        );
        assert!(!chat.remote_chats.get(0).expect("chat").is_editing());
        assert!(chat.take_actions().is_empty());
    }
}

#[test]
fn a_clicked_rich_link_is_queued_for_the_browser_handoff_and_nothing_else() {
    let link = crate::integrations::Inline::Link {
        text: "docs".into(),
        marks: crate::integrations::Marks::default(),
        url: crate::integrations::SafeHttpsUrl::parse("https://example.com/a?b=1").unwrap(),
    };
    let mut chat = ready_chat(vec![rich_own("m1", vec![text_run("see "), link])]);
    let mut toast = ToastState::default();
    // A text run, an out-of-range run and a non-remote index queue nothing.
    for run in [0, 9] {
        chat.handle_message_action(0, MessageAction::OpenRichLink { run }, &mut toast);
    }
    chat.handle_message_action(7, MessageAction::OpenRichLink { run: 1 }, &mut toast);
    assert!(chat.take_remote_link_opens().is_empty());

    chat.handle_message_action(0, MessageAction::OpenRichLink { run: 1 }, &mut toast);
    assert_eq!(
        chat.take_remote_link_opens(),
        vec!["https://example.com/a?b=1".to_owned()]
    );
    assert!(chat.take_remote_link_opens().is_empty());
    assert!(chat.take_actions().is_empty());
}
