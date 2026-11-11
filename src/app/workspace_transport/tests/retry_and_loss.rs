use super::super::queue::{COMMAND_RETRY_BASE, command_retry_delay};
use super::super::{CommandDispatch, WorkspaceTransportController};
use super::{
    RemoteDispatchTransport, ScriptedTransport, remote_channel_post, remote_channel_reply,
    remote_chat_message_delete, remote_chat_message_edit, remote_graph_target, remote_message,
    remote_message_to,
};
use crate::integrations::{
    CommandDisposition, CommandReceipt, ProviderResourceRef, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, SyncInterest, TransportError, WorkspaceCommand, WorkspaceDelta,
    WorkspaceIntentId,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn remote_loss_preserves_queued_local_work_and_resolves_every_remote_intent() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let first_remote = controller.enqueue([remote_message("May have reached Graph")])[0];
    let second_remote = controller.enqueue([remote_message("Must not vanish")])[0];
    let local = controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-chat"),
        body: "Must still stage locally".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }])[0];
    controller
        .pending_dispatches
        .push_back(CommandDispatch::Delivered {
            intent_id: WorkspaceIntentId::for_test(99),
            receipt: CommandReceipt {
                command_id: "already-complete".into(),
                disposition: CommandDisposition::StagedLocally,
                upload: None,
            },
        });

    controller.fallback_to_local_after_remote_loss(
        "test remote worker disappeared",
        TransportError::Unavailable("test remote worker disappeared".into()),
    );
    controller.resolve_lost_remote_dispatch(first_remote);

    let outcomes = controller.dispatch_due();
    assert!(matches!(
        outcomes.as_slice(),
        [
            CommandDispatch::Delivered { intent_id: prior, .. },
            CommandDispatch::DeliveryUncertain { intent_id: first, .. },
            CommandDispatch::ReconciliationFailed { intent_id: first_failed, .. },
            CommandDispatch::Rejected { intent_id: second, .. },
            CommandDispatch::Delivered { intent_id: local_delivered, receipt },
        ] if *prior == WorkspaceIntentId::for_test(99)
            && *first == first_remote
            && *first_failed == first_remote
            && *second == second_remote
            && *local_delivered == local
            && receipt.disposition == CommandDisposition::StagedLocally
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.take_remote_projection_clear_requested());
    let next = controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-next"),
        body: "Intent IDs remain monotonic".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }]);
    assert_eq!(next, [WorkspaceIntentId::for_test(4)]);
}

#[test]
fn lost_remote_worker_marks_a_channel_post_unknown_without_a_refresh_claim() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([remote_channel_post(
        remote_graph_target("team-lost-worker"),
        remote_graph_target("channel-lost-worker"),
        "This post may have reached Microsoft 365",
    )])[0];

    controller.fallback_to_local_after_remote_loss(
        "test remote worker disappeared",
        TransportError::Unavailable("test remote worker disappeared".into()),
    );
    controller.resolve_lost_remote_dispatch(intent_id);

    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        controller.take_remote_projection_clear_for_transport_loss_requested(),
        "only unexpected adapter loss may preserve post recovery state"
    );
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }] if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(!controller.is_microsoft365_active());
}

#[test]
fn lost_remote_worker_marks_a_channel_reply_unknown_without_a_refresh_claim() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([remote_channel_reply(
        remote_graph_target("team-reply-lost-worker"),
        remote_graph_target("channel-reply-lost-worker"),
        remote_graph_target("post-reply-lost-worker"),
        "This reply may have reached Microsoft 365",
    )])[0];

    controller.fallback_to_local_after_remote_loss(
        "test remote worker disappeared",
        TransportError::Unavailable("test remote worker disappeared".into()),
    );
    controller.resolve_lost_remote_dispatch(intent_id);

    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        controller.take_remote_projection_clear_for_transport_loss_requested(),
        "only unexpected adapter loss may preserve reply recovery state"
    );
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }] if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(!controller.is_microsoft365_active());
}

#[test]
fn profile_transition_after_transport_loss_discards_post_recovery_preservation() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    controller.fallback_to_local_after_remote_loss(
        "test remote worker disappeared",
        TransportError::Unavailable("test remote worker disappeared".into()),
    );

    controller
        .synchronize_profile("replacement-profile")
        .expect("the local fallback can initialize a new profile");

    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        !controller.take_remote_projection_clear_for_transport_loss_requested(),
        "a profile transition must not preserve a prior account's remote post draft"
    );
}

#[test]
fn absent_chat_cancellation_removes_only_an_unstarted_remote_intent() {
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([remote_message("Do not post to a hidden chat")])[0];

    controller.cancel_queued_remote_intents([intent_id]);
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [CommandDispatch::Rejected {
            intent_id: rejected,
            error: TransportError::Cancelled,
        }] if *rejected == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .is_empty()
    );
}

#[test]
fn failed_chat_reconciliation_emits_a_terminal_feature_outcome() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:send-reconcile-failure@thread.v2");
    let intent_id = controller.enqueue([remote_message_to(chat.clone(), "Do not replay")])[0];
    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable("lost response".into())),
    );
    let poll = controller
        .pending_remote_interests
        .pop_front()
        .expect("an ambiguous chat send must schedule its exact history poll");

    assert!(matches!(
        controller.handle_completed_remote_poll(
            poll,
            Err(TransportError::Unavailable("history refresh failed".into()))
        ),
        Err(TransportError::Unavailable(_))
    ));
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [
            CommandDispatch::DeliveryUncertain { .. },
            CommandDispatch::ReconciliationFailed {
                intent_id: failed,
                error: TransportError::Unavailable(_),
            }
        ] if *failed == intent_id
    ));
}

#[test]
fn failed_selected_chat_reads_emit_a_row_free_unavailable_marker_but_not_for_cancel_or_reauth() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:selected-failure@thread.v2");
    for error in [
        TransportError::Unavailable("offline".into()),
        TransportError::Throttled {
            retry_after_seconds: Some(3),
        },
    ] {
        for interest in [
            SyncInterest::ChatMessages { chat: chat.clone() },
            SyncInterest::ChatPinnedMessages { chat: chat.clone() },
            SyncInterest::ChatMembers { chat: chat.clone() },
        ] {
            assert!(
                controller
                    .handle_completed_poll(interest.clone(), Err(error.clone()))
                    .is_err()
            );
            let deltas = controller.take_deltas();
            let [delta] = deltas.as_slice() else {
                panic!("one unavailable marker expected for {interest:?}");
            };
            assert!(
                delta
                    .remote_chat_messages
                    .as_ref()
                    .is_some_and(|p| p.is_unavailable())
                    || delta
                        .remote_pinned_chat_messages
                        .as_ref()
                        .is_some_and(|p| p.is_unavailable())
                    || delta
                        .remote_chat_members
                        .as_ref()
                        .is_some_and(|p| p.is_unavailable())
            );
        }
    }
    for error in [
        TransportError::Cancelled,
        TransportError::ReauthenticationRequired,
    ] {
        let _ = controller.handle_completed_poll(
            SyncInterest::ChatMessages { chat: chat.clone() },
            Err(error),
        );
        assert!(controller.take_deltas().is_empty());
    }
    let _ = controller.handle_completed_poll(
        SyncInterest::Teams,
        Err(TransportError::Unavailable("offline".into())),
    );
    assert!(controller.take_deltas().is_empty());
}

#[test]
fn uncertain_chat_send_reconciles_only_against_its_exact_chat_history() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:send-uncertain@thread.v2");
    let intent_id = controller.enqueue([remote_message_to(chat.clone(), "Urgent, once")])[0];
    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable("lost response".into())),
    );
    assert_eq!(controller.pending_message_reconciliations.len(), 1);
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Chats)),
        "the directory preview cannot prove priority and must not reconcile a send"
    );

    // An unrelated directory poll completing first must not settle it.
    controller
        .handle_completed_poll(
            SyncInterest::Chats,
            Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                ..WorkspaceDelta::default()
            }),
        )
        .expect("directory poll succeeds");
    assert_eq!(controller.pending_message_reconciliations.len(), 1);

    let poll = controller
        .pending_remote_interests
        .pop_front()
        .expect("uncertain send must schedule one exact history poll");
    assert!(poll.is_message_reconciliation());
    assert_eq!(
        poll.as_interest(),
        &SyncInterest::ChatMessages { chat: chat.clone() }
    );
    controller
        .handle_completed_remote_poll(
            poll,
            Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChatMessages { chat: chat.clone() }],
                remote_chat_messages: Some(RemoteChatMessagesProjection::for_test(
                    "19:send-uncertain@thread.v2",
                    Vec::new(),
                )),
                ..WorkspaceDelta::default()
            }),
        )
        .expect("target-bound history result completes normally");
    assert!(controller.pending_message_reconciliations.is_empty());
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [
            CommandDispatch::DeliveryUncertain { .. },
            CommandDispatch::ReconciliationCompleted { intent_id: reconciled }
        ] if *reconciled == intent_id
    ));
}

#[test]
fn queued_selected_history_poll_does_not_swallow_a_send_reconciliation_poll() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:send-dedupe@thread.v2");
    controller.request_remote_chat_messages(chat.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);

    let intent_id = controller.enqueue([remote_message_to(chat.clone(), "Keep me")])[0];
    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable("lost response".into())),
    );
    assert_eq!(controller.pending_remote_interests.len(), 2);
    assert_eq!(
        controller
            .pending_remote_interests
            .iter()
            .filter(|poll| poll.is_message_reconciliation())
            .count(),
        1
    );

    // Re-selecting the chat must not remove the reconciliation poll either.
    controller.request_remote_chat_messages(chat);
    assert_eq!(
        controller
            .pending_remote_interests
            .iter()
            .filter(|poll| poll.is_message_reconciliation())
            .count(),
        1
    );
    assert_eq!(controller.pending_message_reconciliations.len(), 1);
}

#[test]
fn queued_commands_retry_in_fifo_order_without_losing_later_intents() {
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let transport = ScriptedTransport {
        outcomes: VecDeque::from([
            Err(TransportError::Unavailable("offline once".into())),
            Ok(()),
            Ok(()),
        ]),
        attempted: Arc::clone(&attempted),
        sequence: 0,
    };
    let mut controller = WorkspaceTransportController::with_local_transport(Box::new(transport));
    let first = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("chat-first"),
        body: "First intent".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };
    let second = WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("chat-second"),
        body: "Second intent".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    };
    controller.enqueue([first.clone(), second.clone()]);
    let start = Instant::now();

    assert!(matches!(
        controller.dispatch_due_at(start).as_slice(),
        [CommandDispatch::RetryScheduled {
            notify_user: true,
            ..
        }]
    ));
    assert_eq!(controller.pending_command_count(), 2);
    assert_eq!(
        attempted
            .lock()
            .expect("test attempt list must not be poisoned")
            .as_slice(),
        std::slice::from_ref(&first)
    );
    assert!(
        controller
            .dispatch_due_at(start + Duration::from_secs(1))
            .is_empty()
    );

    assert!(matches!(
        controller
            .dispatch_due_at(start + COMMAND_RETRY_BASE)
            .as_slice(),
        [
            CommandDispatch::Delivered { .. },
            CommandDispatch::Delivered { .. }
        ]
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(
        attempted
            .lock()
            .expect("test attempt list must not be poisoned")
            .as_slice(),
        [first.clone(), first, second]
    );
}

#[test]
fn queued_command_reports_one_failure_per_retry_sequence_and_reset_drops_it() {
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let transport = ScriptedTransport {
        outcomes: VecDeque::from([
            Err(TransportError::Unavailable("offline".into())),
            Err(TransportError::Unavailable("still offline".into())),
        ]),
        attempted,
        sequence: 0,
    };
    let mut controller = WorkspaceTransportController::with_local_transport(Box::new(transport));
    controller.enqueue([WorkspaceCommand::SendChannelPost {
        team: ProviderResourceRef::local_preview("team"),
        channel: ProviderResourceRef::local_preview("channel"),
        body: "Keep retrying this accepted post".into(),
    }]);
    let start = Instant::now();

    assert!(matches!(
        controller.dispatch_due_at(start).as_slice(),
        [CommandDispatch::RetryScheduled {
            notify_user: true,
            ..
        }]
    ));
    assert!(matches!(
        controller
            .dispatch_due_at(start + COMMAND_RETRY_BASE)
            .as_slice(),
        [CommandDispatch::RetryScheduled {
            notify_user: false,
            ..
        }]
    ));
    assert_eq!(controller.pending_command_count(), 1);

    controller.reset();
    assert_eq!(controller.pending_command_count(), 0);
}

#[test]
fn throttling_never_retries_before_the_provider_requested_delay() {
    assert_eq!(
        command_retry_delay(
            &TransportError::Throttled {
                retry_after_seconds: Some(17),
            },
            1,
        ),
        Duration::from_secs(17)
    );
    assert_eq!(
        command_retry_delay(
            &TransportError::Throttled {
                retry_after_seconds: Some(1),
            },
            3,
        ),
        Duration::from_secs(8)
    );
}

#[test]
fn lost_worker_after_an_edit_or_delete_fails_reconciliation_without_a_replay() {
    for delete in [false, true] {
        let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
            RemoteDispatchTransport {
                polls: Arc::new(Mutex::new(Vec::new())),
                dispatch_threads: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
        ));
        let chat = remote_graph_target("19:lost-mutation@thread.v2");
        let message = remote_graph_target("lost-message");
        let command = if delete {
            remote_chat_message_delete(chat, message)
        } else {
            remote_chat_message_edit(chat, message, "edited")
        };
        let intent_id = controller.enqueue([command])[0];

        controller.fallback_to_local_after_remote_loss(
            "test remote worker disappeared",
            TransportError::Unavailable("test remote worker disappeared".into()),
        );
        controller.resolve_lost_remote_dispatch(intent_id);

        assert!(matches!(
            controller.dispatch_due().as_slice(),
            [
                CommandDispatch::DeliveryUncertain { intent_id: uncertain, .. },
                CommandDispatch::ReconciliationFailed { intent_id: failed, .. },
            ] if *uncertain == intent_id && *failed == intent_id
        ));
        assert_eq!(controller.pending_command_count(), 0);
        assert!(controller.pending_remote_interests.is_empty());
    }
}

fn upload_commands(
    chat: ProviderResourceRef,
) -> (WorkspaceCommand, WorkspaceCommand, WorkspaceCommand) {
    use crate::integrations::{LocalUploadSource, UploadSessionRef};
    (
        WorkspaceCommand::UploadChatFile {
            chat: chat.clone(),
            source: LocalUploadSource::for_test(
                std::path::PathBuf::from("/tmp/picked"),
                10,
                None,
                "a.bin",
            ),
        },
        WorkspaceCommand::ContinueChatUpload {
            chat: chat.clone(),
            session: UploadSessionRef::for_test(1),
        },
        WorkspaceCommand::CancelChatUpload {
            chat,
            session: UploadSessionRef::for_test(1),
        },
    )
}

#[test]
fn a_worker_lost_mid_upload_resolves_the_upload_once_without_a_replay_or_a_refresh() {
    // The simple upload, a session's first command, and a session fragment all
    // end the same way: one delivery-uncertain outcome for the command in
    // flight, a rejection for what was queued behind it, and no reconciliation
    // read (an upload is not a chat message).
    for in_flight in 0..3 {
        let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
            RemoteDispatchTransport {
                polls: Arc::new(Mutex::new(Vec::new())),
                dispatch_threads: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
        ));
        let chat = remote_graph_target("19:lost-upload@thread.v2");
        let (upload, fragment, cancel) = upload_commands(chat);
        let commands = match in_flight {
            0 => [upload, fragment, cancel],
            1 => [fragment, cancel, upload],
            _ => [cancel, upload, fragment],
        };
        let ids = controller.enqueue(commands);

        controller.fallback_to_local_after_remote_loss(
            "test remote worker disappeared",
            TransportError::Unavailable("test remote worker disappeared".into()),
        );
        controller.resolve_lost_remote_dispatch(ids[0]);

        let outcomes = controller.dispatch_due();
        assert!(
            matches!(
                outcomes.as_slice(),
                [
                    CommandDispatch::DeliveryUncertain { intent_id: lost, error: TransportError::Unavailable(_) },
                    CommandDispatch::Rejected { intent_id: second, .. },
                    CommandDispatch::Rejected { intent_id: third, .. },
                ] if *lost == ids[0] && *second == ids[1] && *third == ids[2]
            ),
            "{outcomes:?}"
        );
        assert_eq!(controller.pending_command_count(), 0);
        assert!(controller.pending_remote_interests.is_empty());
        assert!(!controller.is_microsoft365_active());
    }
}
