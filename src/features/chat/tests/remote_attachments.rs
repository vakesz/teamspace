use super::super::remote::{RemoteSendError, RemoteUploadOutcome};
use super::super::{ChatAction, ChatState, RemoteReconciliationResolution};
use super::remote_projection;
use crate::integrations::{
    Capability, RemoteCapabilities, RemoteChatMessage, RemoteChatMessagesProjection,
    UploadProgress, UploadedFileHandle, WorkspaceIntentId,
};
use std::path::PathBuf;

const CHAT_ID: &str = "19:remote-chat@thread.v2";
const GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";
const OTHER_GUID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str, content: &[u8]) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "teamspace-chat-attach-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        Self(path)
    }

    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn chat_with_remote() -> ChatState {
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(remote_projection(CHAT_ID, "Existing"));
    chat.select_remote_conversation(0);
    let _ = chat.take_actions();
    chat
}

fn handle() -> UploadedFileHandle {
    UploadedFileHandle::for_test(7, "q3.pdf", GUID)
}

/// Picks `q3.pdf`, binds its upload and finishes it with `outcome`.
fn attach(chat: &mut ChatState, outcome: RemoteUploadOutcome) -> WorkspaceIntentId {
    let file = TempFile::new("q3.pdf", b"hello");
    chat.attach_remote_file(0, file.path()).expect("pick");
    let mut actions = chat.take_actions();
    assert_eq!(actions.len(), 1);
    let ChatAction::UploadRemoteAttachment { submission, .. } = actions.remove(0) else {
        panic!("an upload action");
    };
    let intent = WorkspaceIntentId::for_test(40);
    assert!(chat.bind_remote_upload(submission, intent));
    assert!(chat.finish_remote_upload(intent, &outcome));
    intent
}

fn own_message(text: &str, references: Vec<(&str, &str)>) -> RemoteChatMessage {
    RemoteChatMessage::for_test("You", "2026-11-06T10:31:00Z", text, true, false)
        .with_attachments_for_test(references)
}

fn history(messages: Vec<RemoteChatMessage>) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(CHAT_ID, messages)
}

fn queue_send(
    chat: &mut ChatState,
) -> (super::super::RemoteSubmissionId, Option<UploadedFileHandle>) {
    chat.queue_remote_message(0).expect("a sendable composer");
    let mut actions = chat.take_actions();
    assert_eq!(actions.len(), 1);
    let ChatAction::SendRemoteMessage {
        submission,
        attachment,
        ..
    } = actions.remove(0)
    else {
        panic!("a send action");
    };
    (submission, attachment)
}

#[test]
fn a_pick_starts_one_upload_and_a_second_is_refused_while_it_runs() {
    let mut chat = chat_with_remote();
    let file = TempFile::new("q3.pdf", b"hello");
    chat.attach_remote_file(0, file.path()).expect("first pick");
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::UploadRemoteAttachment { .. }]
    ));
    let again = TempFile::new("other.pdf", b"x");
    assert_eq!(
        chat.attach_remote_file(0, again.path()),
        Err(RemoteSendError::AttachmentUploading.user_message())
    );
    assert!(chat.take_actions().is_empty());
}

#[test]
fn sending_is_refused_while_the_upload_runs() {
    let mut chat = chat_with_remote();
    let file = TempFile::new("q3.pdf", b"hello");
    chat.attach_remote_file(0, file.path()).expect("pick");
    let _ = chat.take_actions();
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("hi");
    assert!(matches!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::AttachmentUploading)
    ));
    assert!(chat.take_actions().is_empty());
}

#[test]
fn a_bad_pick_is_explained_and_starts_nothing() {
    let mut chat = chat_with_remote();
    let file = TempFile::new("bad#name.pdf", b"x");
    let message = chat.attach_remote_file(0, file.path()).unwrap_err();
    assert!(message.contains("character"));
    assert!(chat.take_actions().is_empty());
    assert!(
        chat.remote_chats
            .get(0)
            .unwrap()
            .attachment_view()
            .is_none()
    );
}

#[test]
fn an_uploaded_file_can_be_sent_with_no_text_and_carries_its_handle() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    let (_, attachment) = queue_send(&mut chat);
    assert_eq!(attachment.expect("the handle").reference_id(), GUID);
    // The file left the composer with the send.
    assert!(
        chat.remote_chats
            .get(0)
            .unwrap()
            .attachment_view()
            .is_none()
    );
}

#[test]
fn a_message_without_text_or_file_is_still_empty() {
    let mut chat = chat_with_remote();
    assert!(matches!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::Empty)
    ));
}

#[test]
fn a_rejected_send_restores_the_draft_with_the_very_same_file() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("the budget");
    let (submission, _) = queue_send(&mut chat);
    let intent = WorkspaceIntentId::for_test(41);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_failed(intent));
    let row = chat.remote_chats.get(0).unwrap();
    assert_eq!(row.draft(), "the budget");
    assert!(row.attachment_view().is_some(), "the file is back");
    // Sending again reuses the handle: nothing is uploaded a second time.
    let (_, attachment) = queue_send(&mut chat);
    assert_eq!(attachment.unwrap().reference_id(), GUID);
}

#[test]
fn an_uncertain_file_send_is_confirmed_only_by_a_row_with_its_reference() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("the budget");
    let (submission, _) = queue_send(&mut chat);
    let intent = WorkspaceIntentId::for_test(42);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));

    // The same words with no file, or another file, confirm nothing.
    assert!(chat.apply_remote_chat_messages_projection(history(vec![
        own_message("the budget", vec![]),
        own_message("the budget", vec![(OTHER_GUID, "other.pdf")]),
    ])));
    // Reconciliation completes unconfirmed: the draft and the file return.
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
    let row = chat.remote_chats.get(0).unwrap();
    assert_eq!(row.draft(), "the budget");
    assert!(row.attachment_view().is_some());
}

#[test]
fn an_uncertain_file_send_is_confirmed_by_its_exact_reference() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("the budget");
    let (submission, _) = queue_send(&mut chat);
    let intent = WorkspaceIntentId::for_test(43);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    assert!(
        chat.apply_remote_chat_messages_projection(history(vec![own_message(
            "the budget",
            vec![(GUID, "q3.pdf")],
        )]))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
    let row = chat.remote_chats.get(0).unwrap();
    assert!(row.attachment_view().is_none());
    assert!(row.draft().is_empty());
}

#[test]
fn a_file_only_send_is_confirmed_by_a_textless_row_with_its_reference() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    let (submission, _) = queue_send(&mut chat);
    let intent = WorkspaceIntentId::for_test(44);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    // A row with text is not the file-only message.
    assert!(
        chat.apply_remote_chat_messages_projection(history(vec![own_message(
            "something else",
            vec![(GUID, "q3.pdf")],
        )]))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
    assert!(
        chat.remote_chats
            .get(0)
            .unwrap()
            .attachment_view()
            .is_some()
    );
    // Try again, and now the textless row appears.
    let (submission, _) = queue_send(&mut chat);
    let intent = WorkspaceIntentId::for_test(45);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    assert!(
        chat.apply_remote_chat_messages_projection(history(vec![own_message(
            "",
            vec![(GUID, "q3.pdf")],
        )]))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::Confirmed)
    );
}

#[test]
fn a_plain_send_is_never_confirmed_by_a_file_row() {
    let mut chat = chat_with_remote();
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("the budget");
    let (submission, attachment) = queue_send(&mut chat);
    assert!(attachment.is_none());
    let intent = WorkspaceIntentId::for_test(46);
    assert!(chat.bind_remote_submission(submission, intent));
    assert!(chat.remote_send_delivery_uncertain(intent));
    assert!(
        chat.apply_remote_chat_messages_projection(history(vec![own_message(
            "the budget",
            vec![(GUID, "q3.pdf")],
        )]))
    );
    assert_eq!(
        chat.complete_remote_send_reconciliation(intent),
        Some(RemoteReconciliationResolution::DraftRestored)
    );
}

#[test]
fn a_recovered_file_needs_an_explicit_accept_before_it_can_be_sent() {
    let mut chat = chat_with_remote();
    attach(
        &mut chat,
        RemoteUploadOutcome::Uploaded(UploadedFileHandle::recovered_for_test(8, "q3.pdf", GUID)),
    );
    assert!(matches!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::AttachmentNotAccepted)
    ));
    assert!(
        chat.remote_chats
            .get_mut(0)
            .unwrap()
            .accept_recovered_attachment()
    );
    let (_, attachment) = queue_send(&mut chat);
    assert!(!attachment.unwrap().is_recovered());
}

#[test]
fn a_name_conflict_and_a_failure_keep_no_sendable_file_and_can_be_dismissed() {
    for outcome in [
        RemoteUploadOutcome::NameConflict,
        RemoteUploadOutcome::Failed("The upload failed. Pick the file again."),
    ] {
        let mut chat = chat_with_remote();
        attach(&mut chat, outcome);
        chat.remote_chats
            .get_mut(0)
            .unwrap()
            .draft_mut()
            .push_str("hi");
        let (_, attachment) = queue_send(&mut chat);
        assert!(attachment.is_none());
        let mut chat = chat_with_remote();
        attach(&mut chat, RemoteUploadOutcome::NameConflict);
        assert!(chat.remote_chats.get_mut(0).unwrap().remove_attachment());
        assert!(
            chat.remote_chats
                .get(0)
                .unwrap()
                .attachment_view()
                .is_none()
        );
    }
}

#[test]
fn a_cancelled_upload_clears_the_composer() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Cancelled);
    assert!(
        chat.remote_chats
            .get(0)
            .unwrap()
            .attachment_view()
            .is_none()
    );
}

#[test]
fn an_attachment_and_a_quote_refuse_each_other() {
    let mut chat = chat_with_remote();
    attach(&mut chat, RemoteUploadOutcome::Uploaded(handle()));
    assert!(matches!(
        chat.remote_chats.toggle_quote_source(0, 0),
        Err(RemoteSendError::AttachmentBlockedByMode)
    ));
}

#[test]
fn attaching_follows_the_capability_and_a_403_turns_it_off() {
    let mut chat = chat_with_remote();
    chat.apply_remote_capabilities(RemoteCapabilities {
        file_attach: Capability::NotGranted,
        ..RemoteCapabilities::default()
    });
    let file = TempFile::new("q3.pdf", b"hello");
    assert!(chat.attach_remote_file(0, file.path()).is_err());
    assert!(chat.take_actions().is_empty());

    let mut chat = chat_with_remote();
    chat.apply_remote_capabilities(RemoteCapabilities {
        file_attach: Capability::Available,
        ..RemoteCapabilities::default()
    });
    chat.mark_file_attach_forbidden();
    assert!(chat.attach_remote_file(0, file.path()).is_err());
}

#[test]
fn a_late_upload_result_for_nothing_awaited_is_ignored() {
    let mut chat = chat_with_remote();
    assert!(!chat.finish_remote_upload(
        WorkspaceIntentId::for_test(99),
        &RemoteUploadOutcome::Uploaded(handle())
    ));
}

#[test]
fn an_unbound_pick_is_pruned_if_its_chat_leaves_the_projection() {
    let mut chat = chat_with_remote();
    let file = TempFile::new("q3.pdf", b"hello");
    chat.attach_remote_file(0, file.path()).expect("pick");
    chat.clear_remote_projection();
    assert!(chat.take_actions().is_empty());
}

/// Picks a file and returns the intent bound to its first command.
fn begin_upload(chat: &mut ChatState, intent: u64) -> WorkspaceIntentId {
    let file = TempFile::new("big.bin", b"hello");
    chat.attach_remote_file(0, file.path()).expect("pick");
    let mut actions = chat.take_actions();
    let ChatAction::UploadRemoteAttachment { submission, .. } = actions.remove(0) else {
        panic!("an upload action");
    };
    let intent = WorkspaceIntentId::for_test(intent);
    assert!(chat.bind_remote_upload(submission, intent));
    intent
}

/// Takes the one continuation the chat asked for and binds it.
fn bind_continuation(chat: &mut ChatState, intent: u64) -> (WorkspaceIntentId, u64) {
    let mut actions = chat.take_actions();
    assert_eq!(actions.len(), 1, "exactly one follow-up");
    let ChatAction::ContinueRemoteUpload {
        submission,
        session,
        ..
    } = actions.remove(0)
    else {
        panic!("a continuation");
    };
    let intent = WorkspaceIntentId::for_test(intent);
    assert!(chat.bind_remote_upload(submission, intent));
    (intent, format!("{session:?}").len() as u64)
}

#[test]
fn a_session_upload_asks_for_each_fragment_in_turn_and_ends_with_the_file() {
    let mut chat = chat_with_remote();
    let mut intent = begin_upload(&mut chat, 50);
    for (step, sent) in [(0_u64, 0_u64), (1, 3_932_160), (2, 7_864_320)] {
        let progress = UploadProgress::for_test(9, sent, 10_000_000);
        assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Progress(progress)));
        // Still uploading; the composer shows how far.
        assert!(matches!(
            chat.remote_chats.get(0).unwrap().attachment_view(),
            Some(super::super::remote::AttachmentView::Uploading { sent: shown, .. }) if shown == sent
        ));
        intent = bind_continuation(&mut chat, 51 + step).0;
    }
    assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Uploaded(handle())));
    assert!(chat.take_actions().is_empty());
    assert!(chat.remote_chats.get(0).unwrap().has_sendable_attachment());
}

#[test]
fn sending_stays_refused_between_fragments() {
    let mut chat = chat_with_remote();
    let intent = begin_upload(&mut chat, 60);
    let progress = UploadProgress::for_test(9, 3_932_160, 10_000_000);
    assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Progress(progress)));
    let _ = bind_continuation(&mut chat, 61);
    chat.remote_chats
        .get_mut(0)
        .unwrap()
        .draft_mut()
        .push_str("hi");
    assert!(matches!(
        chat.queue_remote_message(0),
        Err(RemoteSendError::AttachmentUploading)
    ));
}

#[test]
fn a_failure_after_the_session_opened_cancels_that_session() {
    for outcome in [
        RemoteUploadOutcome::Failed("The upload failed. Pick the file again."),
        RemoteUploadOutcome::NameConflict,
        RemoteUploadOutcome::Cancelled,
    ] {
        let mut chat = chat_with_remote();
        let intent = begin_upload(&mut chat, 70);
        let progress = UploadProgress::for_test(9, 3_932_160, 10_000_000);
        assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Progress(progress)));
        let (next, _) = bind_continuation(&mut chat, 71);
        assert!(chat.finish_remote_upload(next, &outcome));
        let actions = chat.take_actions();
        assert!(
            matches!(
                actions.as_slice(),
                [ChatAction::CancelRemoteUpload {
                    session: Some(_),
                    pending_intent: None,
                    ..
                }]
            ),
            "{actions:?}"
        );
    }
}

#[test]
fn a_failure_before_any_session_has_nothing_to_cancel() {
    let mut chat = chat_with_remote();
    let intent = begin_upload(&mut chat, 80);
    assert!(chat.finish_remote_upload(
        intent,
        &RemoteUploadOutcome::Failed("The upload failed. Pick the file again.")
    ));
    assert!(chat.take_actions().is_empty());
}

#[test]
fn the_user_can_cancel_a_running_upload_and_its_late_fragment_is_ignored() {
    let mut chat = chat_with_remote();
    let intent = begin_upload(&mut chat, 90);
    let progress = UploadProgress::for_test(9, 3_932_160, 10_000_000);
    assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Progress(progress)));
    let (fragment, _) = bind_continuation(&mut chat, 91);

    chat.cancel_remote_upload(0);
    assert!(
        chat.remote_chats
            .get(0)
            .unwrap()
            .attachment_view()
            .is_none()
    );
    let actions = chat.take_actions();
    assert!(matches!(
        actions.as_slice(),
        [ChatAction::CancelRemoteUpload {
            session: Some(_),
            pending_intent: Some(pending),
            ..
        }] if *pending == fragment
    ));
    // The fragment that was already running reports back: nobody waits for it,
    // so the app is told (by `false`) to discard the session.
    let late = UploadProgress::for_test(9, 7_864_320, 10_000_000);
    assert!(!chat.finish_remote_upload(fragment, &RemoteUploadOutcome::Progress(late)));
    assert!(chat.take_actions().is_empty());
    // Cancelling again does nothing.
    chat.cancel_remote_upload(0);
    assert!(chat.take_actions().is_empty());
}

#[test]
fn cancelling_before_the_first_command_is_bound_leaves_no_session_to_cancel() {
    let mut chat = chat_with_remote();
    let file = TempFile::new("big.bin", b"hello");
    chat.attach_remote_file(0, file.path()).expect("pick");
    chat.cancel_remote_upload(0);
    let actions = chat.take_actions();
    // The unbound upload action is pruned or becomes unbindable; the cancel
    // carries neither a session nor an intent.
    assert!(actions.iter().all(|action| !matches!(
        action,
        ChatAction::CancelRemoteUpload {
            session: Some(_),
            ..
        }
    )));
}

#[test]
fn a_worker_lost_mid_upload_leaves_a_failed_composer_not_a_stuck_one() {
    // The controller reports a lost worker as delivery-uncertain.
    let mut chat = chat_with_remote();
    let intent = begin_upload(&mut chat, 100);
    let outcome =
        super::super::upload_failure_reason(&crate::integrations::TransportError::Unavailable(
            "Microsoft Graph message delivery stopped unexpectedly".into(),
        ));
    assert!(chat.finish_remote_upload(intent, &outcome));
    let view = chat.remote_chats.get(0).unwrap().attachment_view();
    assert!(matches!(
        view,
        Some(super::super::remote::AttachmentView::Failed { .. })
    ));
    // It can be dismissed and picked again.
    assert!(chat.remote_chats.get_mut(0).unwrap().remove_attachment());
    let again = TempFile::new("again.bin", b"x");
    assert!(chat.attach_remote_file(0, again.path()).is_ok());
}

#[test]
fn a_worker_lost_between_fragments_fails_the_composer_and_cancels_the_session() {
    let mut chat = chat_with_remote();
    let intent = begin_upload(&mut chat, 110);
    let progress = UploadProgress::for_test(9, 3_932_160, 10_000_000);
    assert!(chat.finish_remote_upload(intent, &RemoteUploadOutcome::Progress(progress)));
    let (fragment, _) = bind_continuation(&mut chat, 111);
    let outcome = super::super::upload_failure_reason(
        &crate::integrations::TransportError::Unavailable("stopped".into()),
    );
    assert!(chat.finish_remote_upload(fragment, &outcome));
    assert!(matches!(
        chat.remote_chats.get(0).unwrap().attachment_view(),
        Some(super::super::remote::AttachmentView::Failed { .. })
    ));
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::CancelRemoteUpload {
            session: Some(_),
            ..
        }]
    ));
}
