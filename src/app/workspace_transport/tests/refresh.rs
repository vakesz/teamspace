use super::super::WorkspaceTransportController;
use super::{SequencedRemoteTransport, remote_graph_target};
use crate::integrations::{SyncInterest, TransportError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn remote_controller() -> WorkspaceTransportController {
    WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
        polls: Arc::new(Mutex::new(Vec::new())),
    }))
}

fn queued(controller: &WorkspaceTransportController, interest: &SyncInterest) -> usize {
    controller
        .pending_remote_interests
        .iter()
        .filter(|poll| poll.as_interest() == interest)
        .count()
}

#[test]
fn repeated_directory_refresh_events_queue_exactly_one_chats_poll() {
    let mut controller = remote_controller();
    assert!(controller.request_remote_chats_refresh());
    assert!(!controller.request_remote_chats_refresh());
    assert!(!controller.request_remote_chats_refresh());
    assert_eq!(queued(&controller, &SyncInterest::Chats), 1);
}

#[test]
fn a_directory_refresh_is_a_no_op_during_a_retry_after_cooldown() {
    let mut controller = remote_controller();
    assert!(
        controller
            .handle_completed_poll_at(
                SyncInterest::Chats,
                Err(TransportError::Throttled {
                    retry_after_seconds: Some(600),
                }),
                Instant::now(),
            )
            .is_err()
    );
    assert!(!controller.request_remote_chats_refresh());
    assert_eq!(queued(&controller, &SyncInterest::Chats), 0);
}

#[test]
fn a_directory_refresh_is_a_no_op_without_an_active_remote_session() {
    let mut controller = WorkspaceTransportController::default();
    assert!(!controller.request_remote_chats_refresh());
    assert!(controller.pending_remote_interests.is_empty());
}

#[test]
fn a_directory_refresh_leaves_delivery_and_reconciliation_polls_alone() {
    let mut controller = remote_controller();
    let chat = remote_graph_target("19:refresh-isolation@thread.v2");
    controller.queue_remote_chat_messages_refresh(chat.clone());
    let delivery = SyncInterest::ChatMessages { chat };
    assert_eq!(queued(&controller, &delivery), 1);

    assert!(controller.request_remote_chats_refresh());
    assert_eq!(queued(&controller, &delivery), 1, "delivery read survives");
    assert_eq!(queued(&controller, &SyncInterest::Chats), 1);
}

#[test]
fn a_selected_chat_refresh_never_replaces_a_delivery_refresh_for_the_same_chat() {
    let mut controller = remote_controller();
    let chat = remote_graph_target("19:refresh-selected@thread.v2");
    controller.queue_remote_chat_messages_refresh(chat.clone());
    controller.request_remote_chat_messages(chat.clone());
    controller.request_remote_chat_messages(chat.clone());

    let polls: Vec<_> = controller.pending_remote_interests.iter().collect();
    assert_eq!(polls.len(), 2, "one delivery read and one selected read");
    assert_eq!(
        polls
            .iter()
            .filter(|poll| poll.is_delivery_chat_messages_refresh())
            .count(),
        1
    );
    assert_eq!(
        polls
            .iter()
            .filter(|poll| poll.is_selected_chat_messages())
            .count(),
        1
    );
}

fn drain(controller: &mut WorkspaceTransportController) -> Vec<super::super::CommandDispatch> {
    let mut outcomes = Vec::new();
    for _ in 0..20_000 {
        outcomes.extend(controller.dispatch_due());
        if !outcomes.is_empty() && !controller.is_polling() {
            break;
        }
        std::thread::yield_now();
    }
    outcomes
}

fn mark_read_controller(
    outcome: Result<crate::integrations::CommandReceipt, TransportError>,
) -> WorkspaceTransportController {
    WorkspaceTransportController::with_remote_transport(Box::new(super::RemoteDispatchTransport {
        polls: Arc::new(Mutex::new(Vec::new())),
        dispatch_threads: Arc::new(Mutex::new(Vec::new())),
        outcomes: std::collections::VecDeque::from([outcome]),
    }))
}

fn mark_read(chat: &str) -> crate::integrations::WorkspaceCommand {
    crate::integrations::WorkspaceCommand::MarkChatRead {
        chat: remote_graph_target(chat),
    }
}

#[test]
fn a_throttled_read_receipt_is_rejected_never_scheduled_for_retry() {
    use super::super::CommandDispatch;
    let mut controller = mark_read_controller(Err(TransportError::Throttled {
        retry_after_seconds: Some(1),
    }));
    let intent = controller.enqueue([mark_read("19:receipt-throttle@thread.v2")])[0];
    let outcomes = drain(&mut controller);
    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::Rejected { intent_id, error: TransportError::Throttled { .. } }]
            if *intent_id == intent
    ));
    assert_eq!(
        controller.pending_command_count(),
        0,
        "no retry stays queued"
    );
}

#[test]
fn an_uncertain_read_receipt_is_not_replayed_or_reconciled() {
    use super::super::CommandDispatch;
    let mut controller = mark_read_controller(Err(TransportError::Unavailable("offline".into())));
    let intent = controller.enqueue([mark_read("19:receipt-uncertain@thread.v2")])[0];
    let outcomes = drain(&mut controller);
    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::DeliveryUncertain { intent_id, .. }] if *intent_id == intent
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(
        controller.pending_remote_interests.is_empty(),
        "no history read or reconciliation follows a receipt"
    );
}

#[test]
fn a_delivered_read_receipt_queues_no_follow_up_read() {
    use super::super::CommandDispatch;
    use crate::integrations::{CommandDisposition, CommandReceipt};
    let mut controller = mark_read_controller(Ok(CommandReceipt {
        command_id: "microsoft365:chat-read-receipt".into(),
        disposition: CommandDisposition::AppliedRemotely,
        upload: None,
    }));
    controller.enqueue([mark_read("19:receipt-ok@thread.v2")]);
    let outcomes = drain(&mut controller);
    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::Delivered { .. }]
    ));
    assert!(controller.pending_remote_interests.is_empty());
}

#[test]
fn a_throttled_upload_is_rejected_never_scheduled_for_retry() {
    use super::super::CommandDispatch;
    use crate::integrations::{LocalUploadSource, UploadSessionRef, WorkspaceCommand};
    let chat = || remote_graph_target("19:upload-throttle@thread.v2");
    let commands = [
        WorkspaceCommand::UploadChatFile {
            chat: chat(),
            source: LocalUploadSource::for_test(
                std::path::PathBuf::from("/tmp/picked"),
                10,
                None,
                "a.bin",
            ),
        },
        WorkspaceCommand::ContinueChatUpload {
            chat: chat(),
            session: UploadSessionRef::for_test(1),
        },
        WorkspaceCommand::CancelChatUpload {
            chat: chat(),
            session: UploadSessionRef::for_test(1),
        },
        WorkspaceCommand::ShareUploadedFileWithChat {
            chat: chat(),
            upload: crate::integrations::UploadedFileHandle::for_test(1, "a.bin", "ref"),
        },
    ];
    for command in commands {
        let mut controller = mark_read_controller(Err(TransportError::Throttled {
            retry_after_seconds: Some(1),
        }));
        let intent = controller.enqueue([command])[0];
        let outcomes = drain(&mut controller);
        assert!(
            matches!(
                outcomes.as_slice(),
                [CommandDispatch::Rejected { intent_id, error: TransportError::Throttled { .. } }]
                    if *intent_id == intent
            ),
            "{outcomes:?}"
        );
        assert_eq!(
            controller.pending_command_count(),
            0,
            "no retry stays queued"
        );
        assert!(controller.next_retry_delay().is_none());
    }
}

#[test]
fn a_throttled_chat_send_is_still_retried_as_before() {
    use super::super::CommandDispatch;
    let mut controller = mark_read_controller(Err(TransportError::Throttled {
        retry_after_seconds: Some(1),
    }));
    controller.enqueue([super::remote_message("still retried")]);
    let outcomes = drain(&mut controller);
    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::RetryScheduled { .. }]
    ));
    assert_eq!(controller.pending_command_count(), 1);
}
