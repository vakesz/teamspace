use super::super::remote_worker::RemotePoll;
use super::super::{CommandDispatch, WorkspaceTransportController};
use super::{
    BlockingRemoteDispatchTransport, RemoteDispatchTransport, remote_channel_post,
    remote_channel_post_reaction, remote_channel_reply, remote_channel_reply_reaction,
    remote_chat_message_delete, remote_chat_message_edit, remote_chat_message_reaction,
    remote_chat_target, remote_graph_target, remote_message, remote_quote_reply,
    wait_for_dispatches,
};
use crate::integrations::{
    CommandDisposition, CommandReceipt, ProviderResourceRef, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, SyncInterest, TransportError, WorkspaceCommand, WorkspaceDelta,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

#[test]
fn remote_target_never_falls_back_to_local_preview_when_unsupported() {
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::from([Err(TransportError::Unsupported(
                "remote send is not consented".into(),
            ))]),
        }));
    let expected_id = controller.enqueue([remote_message("Never stage locally")])[0];

    assert!(controller.dispatch_due().is_empty());
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::Rejected {
            intent_id,
            error: TransportError::Unsupported(_),
        }] if *intent_id == expected_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .len(),
        1
    );
}

#[test]
fn remote_head_blocks_later_local_commands_until_its_worker_returns() {
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        BlockingRemoteDispatchTransport {
            started: started_sender,
            release: release_receiver,
        },
    ));
    let remote_id = controller.enqueue([remote_message("First remotely")])[0];
    let local_id = controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-chat"),
        body: "Must wait behind the remote command".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }])[0];

    assert!(controller.dispatch_due().is_empty());
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );
    assert!(controller.dispatch_due().is_empty());
    assert_eq!(controller.pending_command_count(), 2);

    release_sender
        .send(())
        .expect("the remote worker must still be waiting");
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [
            CommandDispatch::Delivered { intent_id: first, .. },
            CommandDispatch::Delivered { intent_id: second, .. },
        ] if *first == remote_id && *second == local_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
}

#[test]
fn ambiguous_remote_failure_is_not_retried_and_requests_exact_chat_history_reconciliation() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::clone(&polls),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::from([Err(TransportError::Unavailable(
                "the response may have been lost".into(),
            ))]),
        }));
    let expected_id = controller.enqueue([remote_message("Do not duplicate this")])[0];

    assert!(controller.dispatch_due().is_empty());
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::DeliveryUncertain {
            intent_id,
            error: TransportError::Unavailable(_),
        }] if *intent_id == expected_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());

    for _ in 0..10_000 {
        if polls
            .lock()
            .expect("test poll list must not be poisoned")
            .contains(&SyncInterest::ChatMessages {
                chat: remote_chat_target(),
            })
        {
            break;
        }
        let _ = controller.dispatch_due();
        thread::yield_now();
    }
    assert!(
        polls
            .lock()
            .expect("test poll list must not be poisoned")
            .contains(&SyncInterest::ChatMessages {
                chat: remote_chat_target()
            })
    );

    let reconciliation = wait_for_dispatches(&mut controller);
    assert!(matches!(
        reconciliation.as_slice(),
        [CommandDispatch::ReconciliationCompleted { intent_id }]
            if *intent_id == expected_id
    ));
}

#[test]
fn successful_remote_channel_post_refreshes_its_exact_root_feed_not_chats() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-post-success");
    let channel = remote_graph_target("channel-channel-post-success");
    let intent_id = controller.enqueue([remote_channel_post(
        team.clone(),
        channel.clone(),
        "Refresh this exact channel only",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-post".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChannelMessages { team, channel })
    );
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Chats))
    );
    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::Delivered {
            intent_id: delivered,
            receipt,
        }) if *delivered == intent_id
            && receipt.disposition == CommandDisposition::AppliedRemotely
    ));
}

#[test]
fn successful_remote_chat_reaction_refreshes_only_its_exact_selected_history() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:chat-reaction-success@thread.v2");
    let message = remote_graph_target("chat-message-reaction-success");
    let intent_id =
        controller.enqueue([remote_chat_message_reaction(chat.clone(), message, true)])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:chat-reaction".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChatMessages { chat })
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("chat-reaction refresh must be queued")
            .is_delivery_chat_messages_refresh(),
        "a chat reaction delivery must remain distinct from selected-chat work"
    );
    assert!(!controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::Chats
                | SyncInterest::ChannelMessages { .. }
                | SyncInterest::ChannelReplies { .. }
        )
    }));
}

#[test]
fn successful_remote_quote_reply_refreshes_only_its_exact_selected_history() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:quote-success@thread.v2");
    let message = remote_graph_target("quote-source-success");
    let intent_id = controller.enqueue([remote_quote_reply(
        chat.clone(),
        message,
        "Reply with a quote",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:quote-reply".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.pending_message_reconciliations.is_empty());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChatMessages { chat })
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("quote receipt must refresh selected history")
            .is_delivery_chat_messages_refresh()
    );
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Chats))
    );
}

#[test]
fn uncertain_remote_quote_reply_completes_only_after_its_target_bound_history_poll() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:quote-uncertain@thread.v2");
    let message = remote_graph_target("quote-source-uncertain");
    let intent_id = controller.enqueue([remote_quote_reply(
        chat.clone(),
        message,
        "Do not replay this quote",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable(
            "the response may have been lost".into(),
        )),
    );
    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_message_reconciliations.len(), 1);
    let poll = controller
        .pending_remote_interests
        .pop_front()
        .expect("uncertain quote reply must schedule one exact history poll");
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
                    "19:quote-uncertain@thread.v2",
                    Vec::new(),
                )),
                ..WorkspaceDelta::default()
            }),
        )
        .expect("target-bound quote history result must complete normally");
    assert!(controller.pending_message_reconciliations.is_empty());
    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [
            CommandDispatch::DeliveryUncertain { intent_id: uncertain, .. },
            CommandDispatch::ReconciliationCompleted { intent_id: reconciled }
        ] if *uncertain == intent_id && *reconciled == intent_id
    ));
}

#[test]
fn successful_remote_channel_reaction_refreshes_only_its_exact_root_feed() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reaction-success");
    let channel = remote_graph_target("channel-channel-reaction-success");
    let post = remote_graph_target("post-channel-reaction-success");
    let intent_id = controller.enqueue([remote_channel_post_reaction(
        team.clone(),
        channel.clone(),
        post,
        true,
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-reaction".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChannelMessages { team, channel })
    );
    assert!(!controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelReplies { .. }
        )
    }));
}

#[test]
fn successful_remote_channel_reply_reaction_refreshes_only_its_exact_root_replies() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reply-reaction-success");
    let channel = remote_graph_target("channel-channel-reply-reaction-success");
    let post = remote_graph_target("post-channel-reply-reaction-success");
    let reply = remote_graph_target("reply-channel-reply-reaction-success");
    let intent_id = controller.enqueue([remote_channel_reply_reaction(
        team.clone(),
        channel.clone(),
        post.clone(),
        reply,
        true,
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-reply-reaction".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChannelReplies {
            team: team.clone(),
            channel: channel.clone(),
            post: post.clone(),
        })
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("reply-reaction reconciliation must be queued")
            .is_delivery_channel_replies_refresh(),
        "a reply-reaction delivery must remain distinguishable from selected-root work"
    );
    assert!(!controller.pending_remote_interests.iter().any(|interest| {
        matches!(
            interest.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelMessages { .. }
        )
    }));
    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::Delivered {
            intent_id: delivered,
            receipt,
        }) if *delivered == intent_id
            && receipt.disposition == CommandDisposition::AppliedRemotely
    ));
}

#[test]
fn successful_remote_channel_reply_refreshes_its_exact_root_replies_not_chats_or_posts() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reply-success");
    let channel = remote_graph_target("channel-channel-reply-success");
    let post = remote_graph_target("post-channel-reply-success");
    let intent_id = controller.enqueue([remote_channel_reply(
        team.clone(),
        channel.clone(),
        post.clone(),
        "Refresh this exact reply thread only",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-reply".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&SyncInterest::ChannelReplies {
            team: team.clone(),
            channel: channel.clone(),
            post: post.clone(),
        })
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("delivery reconciliation must be queued")
            .is_delivery_channel_replies_refresh(),
        "post-delivery reconciliation must remain distinguishable from a user selection"
    );
    assert!(!controller.pending_remote_interests.iter().any(|interest| {
        matches!(
            interest.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelMessages { .. }
        )
    }));
    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::Delivered {
            intent_id: delivered,
            receipt,
        }) if *delivered == intent_id
            && receipt.disposition == CommandDisposition::AppliedRemotely
    ));
}

fn idle_controller() -> WorkspaceTransportController {
    WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
        polls: Arc::new(Mutex::new(Vec::new())),
        dispatch_threads: Arc::new(Mutex::new(Vec::new())),
        outcomes: VecDeque::new(),
    }))
}

fn mutation_commands(chat: &ProviderResourceRef) -> [WorkspaceCommand; 2] {
    let message = remote_graph_target("mutated-message");
    [
        remote_chat_message_edit(chat.clone(), message.clone(), "edited"),
        remote_chat_message_delete(chat.clone(), message),
    ]
}

#[test]
fn successful_edit_or_delete_refreshes_only_its_exact_chat_history() {
    for index in 0..2 {
        let mut controller = idle_controller();
        let chat = remote_graph_target("19:mutation-success@thread.v2");
        let command = mutation_commands(&chat)[index].clone();
        let intent_id = controller.enqueue([command])[0];

        controller.handle_completed_remote_dispatch(
            intent_id,
            Ok(CommandReceipt {
                command_id: "remote:mutation".into(),
                disposition: CommandDisposition::AppliedRemotely,
                upload: None,
            }),
        );

        assert_eq!(controller.pending_command_count(), 0);
        assert!(controller.pending_message_reconciliations.is_empty());
        assert_eq!(controller.pending_remote_interests.len(), 1);
        let poll = controller
            .pending_remote_interests
            .front()
            .expect("refresh");
        assert_eq!(
            poll.as_interest(),
            &SyncInterest::ChatMessages { chat: chat.clone() }
        );
        assert!(poll.is_delivery_chat_messages_refresh());
    }
}

#[test]
fn uncertain_edit_or_delete_is_never_replayed_and_reconciles_against_its_exact_chat() {
    for index in 0..2 {
        let mut controller = idle_controller();
        let chat = remote_graph_target("19:mutation-uncertain@thread.v2");
        let intent_id = controller.enqueue([mutation_commands(&chat)[index].clone()])[0];

        controller.handle_completed_remote_dispatch(
            intent_id,
            Err(TransportError::Unavailable("response may be lost".into())),
        );

        // Dropped from the FIFO: no automatic replay of a PATCH or POST.
        assert_eq!(controller.pending_command_count(), 0);
        assert_eq!(controller.pending_message_reconciliations.len(), 1);
        assert_eq!(controller.pending_remote_interests.len(), 1);
        let poll = controller
            .pending_remote_interests
            .pop_front()
            .expect("poll");
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
                        "19:mutation-uncertain@thread.v2",
                        Vec::new(),
                    )),
                    ..WorkspaceDelta::default()
                }),
            )
            .expect("exact-chat history completes reconciliation");
        let dispatches = controller.dispatch_due();
        assert!(
            matches!(
                dispatches.as_slice(),
                [
                    CommandDispatch::DeliveryUncertain { intent_id: a, .. },
                    CommandDispatch::ReconciliationCompleted { intent_id: b },
                ] if *a == intent_id && *b == intent_id
            ),
            "{dispatches:?}"
        );
    }
}

#[test]
fn rejected_or_forbidden_edit_or_delete_is_a_terminal_rejection_without_refresh() {
    for error in [
        TransportError::InvalidCommand("renewed the session before delivery; send again".into()),
        TransportError::Unauthorized,
    ] {
        for index in 0..2 {
            let mut controller = idle_controller();
            let chat = remote_graph_target("19:mutation-rejected@thread.v2");
            let intent_id = controller.enqueue([mutation_commands(&chat)[index].clone()])[0];
            controller.handle_completed_remote_dispatch(intent_id, Err(error.clone()));
            assert_eq!(controller.pending_command_count(), 0);
            assert!(controller.pending_remote_interests.is_empty());
            assert!(controller.pending_message_reconciliations.is_empty());
            assert!(matches!(
                controller.dispatch_due().as_slice(),
                [CommandDispatch::Rejected { intent_id: id, error: e }]
                    if *id == intent_id && *e == error
            ));
        }
    }
}
