use super::super::remote_worker::RemotePoll;
use super::super::{CommandDispatch, WorkspaceTransportController};
use super::{
    BlockingRemoteDispatchTransport, RemoteDispatchTransport, ScriptedTransport,
    remote_channel_post, remote_channel_post_reaction, remote_channel_reply,
    remote_channel_reply_reaction, remote_chat_message_reaction, remote_graph_target,
    remote_message, wait_for_dispatches,
};
use crate::integrations::{
    CommandDisposition, CommandReceipt, ProviderResourceRef, RemoteChatMessageImportance,
    SyncInterest, TransportError, WorkspaceCommand,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn ambiguous_remote_channel_post_is_not_replayed_or_chat_reconciled() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::clone(&polls),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::from([Err(TransportError::Unavailable(
                "the response may have been lost".into(),
            ))]),
        }));
    let team = remote_graph_target("team-channel-post-uncertain");
    let channel = remote_graph_target("channel-channel-post-uncertain");
    let expected_interest = SyncInterest::ChannelMessages {
        team: team.clone(),
        channel: channel.clone(),
    };
    let intent_id = controller.enqueue([remote_channel_post(
        team,
        channel,
        "Never automatically send this a second time",
    )])[0];

    assert!(controller.dispatch_due().is_empty());
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }] if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());

    for _ in 0..10_000 {
        let _ = controller.dispatch_due();
        if polls
            .lock()
            .expect("test poll list must not be poisoned")
            .contains(&expected_interest)
        {
            break;
        }
        thread::yield_now();
    }

    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(polls.contains(&expected_interest));
    assert!(!polls.contains(&SyncInterest::Chats));
    drop(polls);
    assert_eq!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .len(),
        1
    );
    assert!(controller.dispatch_due().is_empty());
}

#[test]
fn ambiguous_remote_chat_reaction_is_not_replayed_and_refreshes_only_its_history() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let chat = remote_graph_target("19:chat-reaction-uncertain@thread.v2");
    let message = remote_graph_target("chat-message-reaction-uncertain");
    let expected_interest = SyncInterest::ChatMessages { chat: chat.clone() };
    let intent_id = controller.enqueue([remote_chat_message_reaction(chat, message, false)])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable(
            "the response may have been lost".into(),
        )),
    );

    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }) if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&expected_interest)
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("ambiguous chat-reaction refresh must be queued")
            .is_delivery_chat_messages_refresh(),
        "an ambiguous chat reaction must stay distinct from selected-chat work"
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
fn ambiguous_remote_channel_reaction_is_not_replayed_and_refreshes_only_its_channel() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reaction-uncertain");
    let channel = remote_graph_target("channel-channel-reaction-uncertain");
    let post = remote_graph_target("post-channel-reaction-uncertain");
    let expected_interest = SyncInterest::ChannelMessages {
        team: team.clone(),
        channel: channel.clone(),
    };
    let intent_id =
        controller.enqueue([remote_channel_post_reaction(team, channel, post, false)])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable(
            "the response may have been lost".into(),
        )),
    );

    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }) if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&expected_interest)
    );
    assert!(!controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelReplies { .. }
        )
    }));
}

#[test]
fn ambiguous_remote_channel_reply_reaction_is_not_replayed_and_refreshes_only_its_root() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reply-reaction-uncertain");
    let channel = remote_graph_target("channel-channel-reply-reaction-uncertain");
    let post = remote_graph_target("post-channel-reply-reaction-uncertain");
    let reply = remote_graph_target("reply-channel-reply-reaction-uncertain");
    let expected_interest = SyncInterest::ChannelReplies {
        team: team.clone(),
        channel: channel.clone(),
        post: post.clone(),
    };
    let intent_id = controller.enqueue([remote_channel_reply_reaction(
        team, channel, post, reply, false,
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable(
            "the response may have been lost".into(),
        )),
    );

    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }) if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&expected_interest)
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("ambiguous reply-reaction reconciliation must be queued")
            .is_delivery_channel_replies_refresh(),
        "an ambiguous reply reaction must remain distinguishable from selected-root work"
    );
    assert!(!controller.pending_remote_interests.iter().any(|interest| {
        matches!(
            interest.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelMessages { .. }
        )
    }));
}

#[test]
fn ambiguous_remote_channel_reply_is_not_replayed_and_refreshes_only_its_root() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team = remote_graph_target("team-channel-reply-uncertain");
    let channel = remote_graph_target("channel-channel-reply-uncertain");
    let post = remote_graph_target("post-channel-reply-uncertain");
    let expected_interest = SyncInterest::ChannelReplies {
        team: team.clone(),
        channel: channel.clone(),
        post: post.clone(),
    };
    let intent_id = controller.enqueue([remote_channel_reply(
        team,
        channel,
        post,
        "Never automatically send this reply a second time",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Unavailable(
            "the response may have been lost".into(),
        )),
    );

    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::DeliveryUncertain {
            intent_id: uncertain,
            error: TransportError::Unavailable(_),
        }) if *uncertain == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(&expected_interest)
    );
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("delivery reconciliation must be queued")
            .is_delivery_channel_replies_refresh(),
        "ambiguous delivery reconciliation must not be treated as selected-root work"
    );
    assert!(!controller.pending_remote_interests.iter().any(|interest| {
        matches!(
            interest.as_interest(),
            SyncInterest::Chats | SyncInterest::ChannelMessages { .. }
        )
    }));
}

#[test]
fn throttled_remote_channel_reply_stays_at_the_fifo_head_without_refresh_duplication() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([remote_channel_reply(
        remote_graph_target("team-channel-reply-throttled"),
        remote_graph_target("channel-channel-reply-throttled"),
        remote_graph_target("post-channel-reply-throttled"),
        "Retry only after Graph's retry-after",
    )])[0];

    controller.handle_completed_remote_dispatch(
        intent_id,
        Err(TransportError::Throttled {
            retry_after_seconds: Some(17),
        }),
    );

    assert!(matches!(
        controller.pending_dispatches.front(),
        Some(CommandDispatch::RetryScheduled {
            intent_id: scheduled,
            error: TransportError::Throttled {
                retry_after_seconds: Some(17),
            },
            notify_user: true,
        }) if *scheduled == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 1);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(
        controller
            .next_retry_delay()
            .is_some_and(|delay| delay >= Duration::from_secs(16))
    );
}

#[test]
fn remote_channel_post_refresh_does_not_displace_files_or_replies() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let files_team = remote_graph_target("team-files-stays-queued");
    let files_channel = remote_graph_target("channel-files-stays-queued");
    let replies_team = remote_graph_target("team-replies-stays-queued");
    let replies_channel = remote_graph_target("channel-replies-stays-queued");
    let replies_post = remote_graph_target("post-replies-stays-queued");
    controller.request_remote_channel_files(files_team.clone(), files_channel.clone());
    controller.request_remote_channel_replies(
        replies_team.clone(),
        replies_channel.clone(),
        replies_post.clone(),
    );

    let post_team = remote_graph_target("team-post-refresh-isolated");
    let post_channel = remote_graph_target("channel-post-refresh-isolated");
    let intent_id = controller.enqueue([remote_channel_post(
        post_team.clone(),
        post_channel.clone(),
        "Keep independent channel reads intact",
    )])[0];
    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-post-isolation".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelFiles {
                    team: files_team.clone(),
                    channel: files_channel.clone(),
                })
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelReplies {
                    team: replies_team.clone(),
                    channel: replies_channel.clone(),
                    post: replies_post.clone(),
                })
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelMessages {
                    team: post_team.clone(),
                    channel: post_channel.clone(),
                })
    );
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Chats))
    );
}

#[test]
fn remote_channel_reply_refresh_does_not_displace_selected_reads_or_files() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let messages_team = remote_graph_target("team-messages-stays-queued");
    let messages_channel = remote_graph_target("channel-messages-stays-queued");
    let files_team = remote_graph_target("team-files-stays-queued-for-reply");
    let files_channel = remote_graph_target("channel-files-stays-queued-for-reply");
    let selected_team = remote_graph_target("team-selected-replies-stays-queued");
    let selected_channel = remote_graph_target("channel-selected-replies-stays-queued");
    let selected_post = remote_graph_target("post-selected-replies-stays-queued");
    controller.request_remote_channel_messages(messages_team.clone(), messages_channel.clone());
    controller.request_remote_channel_files(files_team.clone(), files_channel.clone());
    controller.request_remote_channel_replies(
        selected_team.clone(),
        selected_channel.clone(),
        selected_post.clone(),
    );

    let reply_team = remote_graph_target("team-reply-refresh-isolated");
    let reply_channel = remote_graph_target("channel-reply-refresh-isolated");
    let reply_post = remote_graph_target("post-reply-refresh-isolated");
    let intent_id = controller.enqueue([remote_channel_reply(
        reply_team.clone(),
        reply_channel.clone(),
        reply_post.clone(),
        "Keep independent reads intact",
    )])[0];
    controller.handle_completed_remote_dispatch(
        intent_id,
        Ok(CommandReceipt {
            command_id: "remote:channel-reply-isolation".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        }),
    );

    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelMessages {
                    team: messages_team.clone(),
                    channel: messages_channel.clone(),
                })
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelFiles {
                    team: files_team.clone(),
                    channel: files_channel.clone(),
                })
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelReplies {
                    team: selected_team.clone(),
                    channel: selected_channel.clone(),
                    post: selected_post.clone(),
                })
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| poll.as_interest()
                == &SyncInterest::ChannelReplies {
                    team: reply_team.clone(),
                    channel: reply_channel.clone(),
                    post: reply_post.clone(),
                })
    );
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Chats))
    );
}

#[test]
fn local_channel_post_delivery_keeps_remote_refreshes_absent() {
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([WorkspaceCommand::SendChannelPost {
        team: ProviderResourceRef::local_preview("local-team"),
        channel: ProviderResourceRef::local_preview("local-channel"),
        body: "Retain local preview behavior".into(),
    }])[0];

    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [CommandDispatch::Delivered {
            intent_id: delivered,
            receipt,
        }] if *delivered == intent_id
            && receipt.disposition == CommandDisposition::StagedLocally
    ));
    assert!(controller.pending_remote_interests.is_empty());
    assert!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .is_empty()
    );
}

#[test]
fn local_channel_reply_delivery_keeps_remote_refreshes_absent() {
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::new(),
        }));
    let intent_id = controller.enqueue([WorkspaceCommand::ReplyToChannelPost {
        team: ProviderResourceRef::local_preview("local-team"),
        channel: ProviderResourceRef::local_preview("local-channel"),
        post: ProviderResourceRef::local_preview("local-post"),
        body: "Retain local preview behavior".into(),
    }])[0];

    assert!(matches!(
        controller.dispatch_due().as_slice(),
        [CommandDispatch::Delivered {
            intent_id: delivered,
            receipt,
        }] if *delivered == intent_id
            && receipt.disposition == CommandDisposition::StagedLocally
    ));
    assert!(controller.pending_remote_interests.is_empty());
    assert!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .is_empty()
    );
}

#[test]
fn throttled_remote_send_stays_at_the_fifo_head_for_a_safe_retry() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::from([Err(TransportError::Throttled {
                retry_after_seconds: Some(17),
            })]),
        }));
    let expected_id = controller.enqueue([remote_message("Retry only after 429")])[0];

    assert!(controller.dispatch_due().is_empty());
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::RetryScheduled {
            intent_id,
            error: TransportError::Throttled {
                retry_after_seconds: Some(17),
            },
            notify_user: true,
        }] if *intent_id == expected_id
    ));
    assert_eq!(controller.pending_command_count(), 1);
    assert!(
        controller
            .next_retry_delay()
            .is_some_and(|delay| delay >= Duration::from_secs(16))
    );
}

#[test]
fn in_flight_remote_work_does_not_request_a_zero_delay_retry_frame() {
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        BlockingRemoteDispatchTransport {
            started: started_sender,
            release: release_receiver,
        },
    ));
    controller.enqueue([remote_message("Keep the worker repaint bounded")]);

    assert!(controller.dispatch_due().is_empty());
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );
    assert!(controller.is_polling());
    assert!(controller.next_retry_delay().is_none());
    release_sender
        .send(())
        .expect("the remote worker must still be waiting");
}

#[test]
fn enormous_provider_retry_after_rejects_instead_of_wrapping_to_an_immediate_retry() {
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_local_transport(Box::new(ScriptedTransport {
            outcomes: VecDeque::from([Err(TransportError::Throttled {
                retry_after_seconds: Some(u64::MAX),
            })]),
            attempted: Arc::clone(&attempted),
            sequence: 0,
        }));
    let intent_id = controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-chat"),
        body: "Do not wrap a retry deadline".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }])[0];

    assert!(matches!(
        controller.dispatch_due_at(Instant::now()).as_slice(),
        [CommandDispatch::Rejected {
            intent_id: rejected,
            error: TransportError::Unavailable(_),
        }] if *rejected == intent_id
    ));
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.next_retry_delay().is_none());
    assert_eq!(
        attempted
            .lock()
            .expect("test attempt list must not be poisoned")
            .len(),
        1
    );
}
