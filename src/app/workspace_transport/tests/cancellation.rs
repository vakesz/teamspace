use super::super::WorkspaceTransportController;
use super::super::remote_worker::{RemotePoll, RemoteWork, remote_worker_name};
use super::{
    ChannelSelectionCancellationTransport, ChatHistorySelectionCancellationTransport,
    OneDriveFilePreviewSelectionCancellationTransport, PinnedChatSelectionCancellationTransport,
    RecordingTransport, RemoteDispatchTransport, ReplyRefreshIsolationTransport,
    remote_graph_target,
};
use crate::integrations::SyncInterest;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

#[test]
fn latest_selected_channel_cancels_only_the_obsolete_channel_poll() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let channel_message_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        ChannelSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            channel_message_polls: Arc::clone(&channel_message_polls),
            channel_files_polls: Arc::new(AtomicUsize::new(0)),
            channel_reply_polls: Arc::new(AtomicUsize::new(0)),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    controller
        .synchronize_profile("remote-profile")
        .expect("identity worker must start");
    for _ in 0..10_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("remote bootstrap must succeed");
        if !controller.is_polling()
            && polls
                .lock()
                .expect("test poll list must not be poisoned")
                .len()
                == 5
        {
            break;
        }
        thread::yield_now();
    }
    let _ = controller.take_deltas();

    let team_a = remote_graph_target("remote-team-a");
    let channel_a = remote_graph_target("remote-channel-a");
    let team_b = remote_graph_target("remote-team-b");
    let channel_b = remote_graph_target("remote-channel-b");
    controller.request_remote_channel_messages(team_a.clone(), channel_a.clone());
    controller
        .synchronize_profile("remote-profile")
        .expect("selected channel worker must start");
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );

    controller.request_remote_channel_messages(team_b.clone(), channel_b.clone());
    controller.request_remote_channel_messages(team_b.clone(), channel_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelMessages { team, channel })
            if team == &team_b && channel == &channel_b
    ));

    for _ in 0..20_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("replacement selected channel read must succeed");
        if !controller.is_polling() && channel_message_polls.load(Ordering::Relaxed) == 2 {
            break;
        }
        thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(channel_message_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(controller.is_microsoft365_active());
    let deltas = controller.take_deltas();
    assert_eq!(deltas.len(), 1);
    assert!(matches!(
        deltas[0].completed.as_slice(),
        [SyncInterest::ChannelMessages { team, channel }]
            if team == &team_b && channel == &channel_b
    ));
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::Identity,
            SyncInterest::Chats,
            SyncInterest::Teams,
            SyncInterest::Calendar,
            SyncInterest::Files,
            SyncInterest::ChannelMessages { team, channel },
            SyncInterest::ChannelMessages { team: final_team, channel: final_channel },
        ] if team == &team_a
            && channel == &channel_a
            && final_team == &team_b
            && final_channel == &channel_b
    ));
}

#[test]
fn latest_selected_channel_reply_cancels_only_the_obsolete_reply_poll() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let channel_reply_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        ChannelSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            channel_message_polls: Arc::new(AtomicUsize::new(0)),
            channel_files_polls: Arc::new(AtomicUsize::new(0)),
            channel_reply_polls: Arc::clone(&channel_reply_polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    controller
        .synchronize_profile("remote-profile")
        .expect("identity worker must start");
    for _ in 0..10_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("remote bootstrap must succeed");
        if !controller.is_polling()
            && polls
                .lock()
                .expect("test poll list must not be poisoned")
                .len()
                == 5
        {
            break;
        }
        thread::yield_now();
    }
    let _ = controller.take_deltas();

    let team_a = remote_graph_target("remote-team-a");
    let channel_a = remote_graph_target("remote-channel-a");
    let post_a = remote_graph_target("remote-post-a");
    let team_b = remote_graph_target("remote-team-b");
    let channel_b = remote_graph_target("remote-channel-b");
    let post_b = remote_graph_target("remote-post-b");
    controller.request_remote_channel_replies(team_a.clone(), channel_a.clone(), post_a.clone());
    controller
        .synchronize_profile("remote-profile")
        .expect("selected reply worker must start");
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );

    controller.request_remote_channel_replies(team_b.clone(), channel_b.clone(), post_b.clone());
    controller.request_remote_channel_replies(team_b.clone(), channel_b.clone(), post_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        }) if team == &team_b && channel == &channel_b && post == &post_b
    ));

    for _ in 0..20_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("replacement selected reply read must succeed");
        if !controller.is_polling() && channel_reply_polls.load(Ordering::Relaxed) == 2 {
            break;
        }
        thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(channel_reply_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(controller.is_microsoft365_active());
    let deltas = controller.take_deltas();
    assert_eq!(deltas.len(), 1);
    assert!(matches!(
        deltas[0].completed.as_slice(),
        [SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        }] if team == &team_b && channel == &channel_b && post == &post_b
    ));
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::Identity,
            SyncInterest::Chats,
            SyncInterest::Teams,
            SyncInterest::Calendar,
            SyncInterest::Files,
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            },
            SyncInterest::ChannelReplies {
                team: final_team,
                channel: final_channel,
                post: final_post,
            },
        ] if team == &team_a
            && channel == &channel_a
            && post == &post_a
            && final_team == &team_b
            && final_channel == &channel_b
            && final_post == &post_b
    ));
}

#[test]
fn selecting_new_root_preserves_queued_delivery_reply_refresh() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let team_a = remote_graph_target("queued-refresh-team-a");
    let channel_a = remote_graph_target("queued-refresh-channel-a");
    let post_a = remote_graph_target("queued-refresh-post-a");
    let team_b = remote_graph_target("queued-refresh-team-b");
    let channel_b = remote_graph_target("queued-refresh-channel-b");
    let post_b = remote_graph_target("queued-refresh-post-b");

    controller.request_remote_channel_replies(team_a.clone(), channel_a.clone(), post_a.clone());
    controller.queue_remote_channel_replies_refresh(
        team_a.clone(),
        channel_a.clone(),
        post_a.clone(),
    );
    // The app drains this feature-clear signal before it stages B's new
    // selected-root request.
    controller.cancel_remote_channel_replies();
    controller.request_remote_channel_replies(team_b.clone(), channel_b.clone(), post_b.clone());

    assert_eq!(controller.pending_remote_interests.len(), 2);
    let refresh = controller
        .pending_remote_interests
        .front()
        .expect("delivery refresh must remain queued");
    assert_eq!(
        refresh.as_interest(),
        &SyncInterest::ChannelReplies {
            team: team_a,
            channel: channel_a,
            post: post_a,
        }
    );
    assert!(
        refresh.is_delivery_channel_replies_refresh(),
        "selection replacement must retain delivery reconciliation as delivery-owned work"
    );

    let selected_b = controller
        .pending_remote_interests
        .get(1)
        .expect("new selected root must be queued after the retained refresh");
    assert_eq!(
        selected_b.as_interest(),
        &SyncInterest::ChannelReplies {
            team: team_b,
            channel: channel_b,
            post: post_b,
        }
    );
    assert!(selected_b.is_selected_channel_replies());
}

#[test]
fn selecting_new_root_preserves_active_delivery_reply_refresh() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        ReplyRefreshIsolationTransport {
            polls: Arc::clone(&polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
            release: release_receiver,
            block_first_reply: true,
        },
    ));
    let team_a = remote_graph_target("active-refresh-team-a");
    let channel_a = remote_graph_target("active-refresh-channel-a");
    let post_a = remote_graph_target("active-refresh-post-a");
    let team_b = remote_graph_target("active-refresh-team-b");
    let channel_b = remote_graph_target("active-refresh-channel-b");
    let post_b = remote_graph_target("active-refresh-post-b");

    controller.queue_remote_channel_replies_refresh(
        team_a.clone(),
        channel_a.clone(),
        post_a.clone(),
    );
    controller
        .start_next_remote_poll()
        .expect("delivery refresh worker must start");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("delivery refresh must become active");

    // Match the app's feature-clear ordering: cancellation runs before B's
    // selection request, but must apply only to selection-owned polls.
    controller.cancel_remote_channel_replies();
    controller.request_remote_channel_replies(team_b.clone(), channel_b.clone(), post_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    let selected_b = controller
        .pending_remote_interests
        .front()
        .expect("new selected root must queue behind the active refresh");
    assert_eq!(
        selected_b.as_interest(),
        &SyncInterest::ChannelReplies {
            team: team_b.clone(),
            channel: channel_b.clone(),
            post: post_b.clone(),
        }
    );
    assert!(selected_b.is_selected_channel_replies());

    release_sender
        .send(())
        .expect("delivery refresh worker must still be waiting");
    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(
        !controller.is_polling(),
        "delivery refresh must return its adapter"
    );
    assert!(
        !cancellation_observed.load(Ordering::Relaxed),
        "changing the selected root must not cancel the active delivery refresh"
    );

    controller
        .start_next_remote_poll()
        .expect("selected-root poll must follow the delivery refresh serially");
    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(!controller.is_polling(), "selected-root poll must finish");
    assert!(controller.pending_remote_interests.is_empty());

    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            },
            SyncInterest::ChannelReplies {
                team: selected_team,
                channel: selected_channel,
                post: selected_post,
            },
        ] if team == &team_a
            && channel == &channel_a
            && post == &post_a
            && selected_team == &team_b
            && selected_channel == &channel_b
            && selected_post == &post_b
    ));
}

#[test]
fn replacing_selected_chat_history_cancels_only_chat_history_not_global_chats() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let chat_message_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        ChatHistorySelectionCancellationTransport {
            polls: Arc::clone(&polls),
            chat_message_polls: Arc::clone(&chat_message_polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    let chat_a = remote_graph_target("19:history-a@thread.v2");
    let chat_b = remote_graph_target("19:history-b@thread.v2");

    controller.request_remote_chat_messages(chat_a.clone());
    controller
        .start_next_remote_poll()
        .expect("selected chat history worker must start");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("first selected chat history must become active");

    // A global directory refresh has a separate purpose and must survive both
    // the cancellation of A and the queued selection of B.
    controller.queue_remote_chat_refresh();
    controller.request_remote_chat_messages(chat_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 2);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::Chats)
    ));
    let selected_b = controller
        .pending_remote_interests
        .get(1)
        .expect("B must remain queued behind the global chat refresh");
    assert_eq!(
        selected_b.as_interest(),
        &SyncInterest::ChatMessages {
            chat: chat_b.clone(),
        }
    );
    assert!(selected_b.is_selected_chat_messages());

    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(
        cancellation_observed.load(Ordering::Relaxed),
        "replacing A with B must cancel the active selected-chat history"
    );
    assert!(!controller.is_polling());

    // The selected read has priority over the directory refresh queued
    // before it, so B runs first and the global Chats refresh still follows.
    controller
        .start_next_remote_poll()
        .expect("replacement selected-chat history must start first");
    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    controller
        .start_next_remote_poll()
        .expect("the independent global Chats refresh must still start");
    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }

    assert_eq!(chat_message_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::ChatMessages { chat: first },
            SyncInterest::ChatMessages { chat: second },
            SyncInterest::Chats,
        ] if first == &chat_a && second == &chat_b
    ));
}

#[test]
fn replacing_selected_pinned_messages_cancels_only_pins_not_history_or_global_chats() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let pinned_message_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        PinnedChatSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            pinned_message_polls: Arc::clone(&pinned_message_polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    let pins_a = remote_graph_target("19:pins-a@thread.v2");
    let pins_b = remote_graph_target("19:pins-b@thread.v2");
    let selected_history = remote_graph_target("19:history-kept@thread.v2");

    controller.request_remote_chat_pinned_messages(pins_a.clone());
    controller
        .start_next_remote_poll()
        .expect("selected pinned-message worker must start");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("first selected pinned-message read must become active");

    // Both the global directory and main history are separate read purposes.
    // Replacing the contextual pins target must leave them queued unchanged.
    controller.queue_remote_chat_refresh();
    controller.request_remote_chat_messages(selected_history.clone());
    controller.request_remote_chat_pinned_messages(pins_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 3);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::Chats)
    ));
    assert!(
        controller
            .pending_remote_interests
            .get(1)
            .expect("main selected history must remain queued")
            .is_selected_chat_messages()
    );
    assert_eq!(
        controller
            .pending_remote_interests
            .get(1)
            .expect("main selected history must retain its target")
            .as_interest(),
        &SyncInterest::ChatMessages {
            chat: selected_history.clone(),
        }
    );
    let replacement = controller
        .pending_remote_interests
        .get(2)
        .expect("replacement pinned-message read must remain queued");
    assert!(replacement.is_selected_chat_pinned_messages());
    assert_eq!(
        replacement.as_interest(),
        &SyncInterest::ChatPinnedMessages {
            chat: pins_b.clone(),
        }
    );

    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(
        cancellation_observed.load(Ordering::Relaxed),
        "replacing the pins target must cancel only the active pins read"
    );
    assert!(!controller.is_polling());

    for _ in 0..3 {
        controller
            .start_next_remote_poll()
            .expect("each preserved read must run after the cancelled pins read");
        for _ in 0..20_000 {
            controller.collect_remote_work();
            if !controller.is_polling() {
                break;
            }
            thread::yield_now();
        }
        assert!(!controller.is_polling());
    }

    assert_eq!(pinned_message_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::ChatPinnedMessages { chat: first },
            SyncInterest::ChatMessages { chat: history },
            SyncInterest::ChatPinnedMessages { chat: second },
            SyncInterest::Chats,
        ] if first == &pins_a && history == &selected_history && second == &pins_b
    ));
}

#[test]
fn reopening_same_selected_pinned_messages_after_cancellation_queues_replacement() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let pinned_message_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        PinnedChatSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            pinned_message_polls: Arc::clone(&pinned_message_polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    let pins_chat = remote_graph_target("19:reopened-pins@thread.v2");

    controller.request_remote_chat_pinned_messages(pins_chat.clone());
    controller
        .start_next_remote_poll()
        .expect("the initial pinned-message worker must start");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("the initial pinned-message worker must become active");

    // Closing the panel marks the active read as cancelled, but reopening it
    // before the worker returns must queue a fresh read for the same target.
    controller.cancel_remote_chat_pinned_messages();
    controller.request_remote_chat_pinned_messages(pins_chat.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert_eq!(
        controller
            .pending_remote_interests
            .front()
            .expect("the reopened pins panel must retain a replacement read")
            .as_interest(),
        &SyncInterest::ChatPinnedMessages {
            chat: pins_chat.clone(),
        }
    );

    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(
        cancellation_observed.load(Ordering::Relaxed),
        "closing the panel must cancel the original selected pins read"
    );
    assert!(!controller.is_polling());

    controller
        .start_next_remote_poll()
        .expect("the reopened pins panel must start its replacement read");
    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }

    assert_eq!(pinned_message_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::ChatPinnedMessages { chat: initial },
            SyncInterest::ChatPinnedMessages { chat: replacement },
        ] if initial == &pins_chat && replacement == &pins_chat
    ));
}

#[test]
fn pinned_message_cancellation_preserves_history_delivery_and_other_selected_reads() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let delivery_chat = remote_graph_target("19:delivery-history@thread.v2");
    let selected_chat = remote_graph_target("19:selected-history@thread.v2");
    let pins_chat = remote_graph_target("19:pins-history@thread.v2");
    let preview_item = remote_graph_target("drive-item-kept-alongside-pins");

    controller.queue_remote_chat_messages_refresh(delivery_chat.clone());
    controller.request_remote_chat_messages(selected_chat.clone());
    controller.request_remote_file_preview(preview_item.clone());
    controller.request_remote_chat_pinned_messages(pins_chat.clone());
    controller.request_remote_chat_pinned_messages(pins_chat);
    assert_eq!(
        controller.pending_remote_interests.len(),
        4,
        "the same contextual pins target must deduplicate"
    );

    controller.cancel_remote_chat_pinned_messages();
    assert_eq!(controller.pending_remote_interests.len(), 3);
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("delivery refresh must remain queued")
            .is_delivery_chat_messages_refresh()
    );
    assert!(
        controller
            .pending_remote_interests
            .get(1)
            .expect("selected history must remain queued")
            .is_selected_chat_messages()
    );
    assert!(
        controller
            .pending_remote_interests
            .get(2)
            .expect("unrelated selected file preview must remain queued")
            .is_selected_onedrive_file_preview()
    );
    assert_eq!(
        controller
            .pending_remote_interests
            .get(2)
            .expect("unrelated selected file preview must retain its target")
            .as_interest(),
        &SyncInterest::OneDriveFilePreview { item: preview_item }
    );
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .all(|pending| !pending.is_selected_chat_pinned_messages())
    );
}

#[test]
fn local_preview_never_queues_a_remote_pinned_messages_read() {
    let mut controller =
        WorkspaceTransportController::with_local_transport(Box::new(RecordingTransport {
            dispatches: Arc::new(AtomicUsize::new(0)),
        }));

    controller.request_remote_chat_pinned_messages(remote_graph_target("19:pins@thread.v2"));

    assert!(
        controller.pending_remote_interests.is_empty(),
        "a local adapter must not receive or stage a Microsoft Graph pins read"
    );
}

#[test]
fn pinned_messages_worker_name_is_generic_and_identifier_free() {
    let poll = RemotePoll::selected_chat_pinned_messages(SyncInterest::ChatPinnedMessages {
        chat: remote_graph_target("19:private-pinned-chat@thread.v2"),
    });

    let name = remote_worker_name(&RemoteWork::Poll(poll));

    assert_eq!(name, "teamspace-graph-chat-pinned-messages");
    assert!(
        !name.contains("private-pinned-chat"),
        "an opaque selected chat target must never enter a worker thread name"
    );
}

#[test]
fn selected_chat_cancellation_preserves_a_delivery_reaction_history_refresh() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let reaction_chat = remote_graph_target("19:reaction-refresh@thread.v2");
    let selected_chat = remote_graph_target("19:selected-history@thread.v2");

    controller.queue_remote_chat_messages_refresh(reaction_chat.clone());
    controller.request_remote_chat_messages(selected_chat.clone());
    assert_eq!(controller.pending_remote_interests.len(), 2);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChatMessages { chat }) if chat == &reaction_chat
    ));
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("delivery refresh remains queued")
            .is_delivery_chat_messages_refresh()
    );
    assert!(matches!(
        controller
            .pending_remote_interests
            .get(1)
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChatMessages { chat }) if chat == &selected_chat
    ));
    assert!(
        controller
            .pending_remote_interests
            .get(1)
            .expect("selected history remains independently queued")
            .is_selected_chat_messages()
    );

    controller.cancel_remote_chat_messages();
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert!(
        controller
            .pending_remote_interests
            .front()
            .expect("selection cancellation cannot remove delivery refresh")
            .is_delivery_chat_messages_refresh()
    );
}

#[test]
fn selected_onedrive_file_preview_deduplicates_and_cancel_preserves_other_polls() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::new(Mutex::new(Vec::new())),
            outcomes: VecDeque::new(),
        }));
    let item = remote_graph_target("onedrive-preview-dedup");
    let chat = remote_graph_target("onedrive-preview-chat");
    let team = remote_graph_target("onedrive-preview-team");
    let channel = remote_graph_target("onedrive-preview-channel");

    controller
        .pending_remote_interests
        .push_back(RemotePoll::interest(SyncInterest::Files));
    controller.request_remote_chat_messages(chat.clone());
    controller.request_remote_channel_messages(team.clone(), channel.clone());
    controller.request_remote_file_preview(item.clone());
    controller.request_remote_file_preview(item.clone());

    assert_eq!(controller.pending_remote_interests.len(), 4);
    let preview = controller
        .pending_remote_interests
        .back()
        .expect("one deduplicated selected file preview must remain queued");
    assert_eq!(
        preview.as_interest(),
        &SyncInterest::OneDriveFilePreview { item: item.clone() }
    );
    assert!(preview.is_selected_onedrive_file_preview());
    let worker_name = remote_worker_name(&RemoteWork::Poll(preview.clone()));
    assert_eq!(worker_name, "teamspace-graph-onedrive-file-preview");
    assert!(
        !worker_name.contains("onedrive-preview-dedup"),
        "opaque item identifiers must never enter worker thread names"
    );

    controller.cancel_remote_file_preview();

    assert_eq!(controller.pending_remote_interests.len(), 3);
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::Files))
    );
    assert!(controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::ChatMessages { chat: selected } if selected == &chat
        )
    }));
    assert!(controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::ChannelMessages {
                team: selected_team,
                channel: selected_channel,
            } if selected_team == &team && selected_channel == &channel
        )
    }));
    assert!(
        !controller
            .pending_remote_interests
            .iter()
            .any(RemotePoll::is_selected_onedrive_file_preview)
    );
}

#[test]
fn replacing_selected_onedrive_file_preview_cancels_only_active_preview() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let preview_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        OneDriveFilePreviewSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            preview_polls: Arc::clone(&preview_polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    let item_a = remote_graph_target("onedrive-preview-a");
    let item_b = remote_graph_target("onedrive-preview-b");
    let chat = remote_graph_target("onedrive-preview-unrelated-chat");
    let team = remote_graph_target("onedrive-preview-unrelated-team");
    let channel = remote_graph_target("onedrive-preview-unrelated-channel");

    controller.request_remote_file_preview(item_a.clone());
    controller
        .start_next_remote_poll()
        .expect("selected file preview worker must start");
    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("first selected file preview must become active");

    controller
        .pending_remote_interests
        .push_back(RemotePoll::interest(SyncInterest::Files));
    controller.request_remote_chat_messages(chat.clone());
    controller.request_remote_channel_messages(team.clone(), channel.clone());
    controller.request_remote_file_preview(item_b.clone());
    controller.request_remote_file_preview(item_b.clone());

    assert_eq!(controller.pending_remote_interests.len(), 4);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::Files)
    ));
    assert!(controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::ChatMessages { chat: selected } if selected == &chat
        )
    }));
    assert!(controller.pending_remote_interests.iter().any(|poll| {
        matches!(
            poll.as_interest(),
            SyncInterest::ChannelMessages {
                team: selected_team,
                channel: selected_channel,
            } if selected_team == &team && selected_channel == &channel
        )
    }));
    assert!(matches!(
        controller
            .pending_remote_interests
            .back()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::OneDriveFilePreview { item }) if item == &item_b
    ));
    assert!(
        controller
            .pending_remote_interests
            .back()
            .expect("replacement preview must retain selected-preview provenance")
            .is_selected_onedrive_file_preview()
    );

    for _ in 0..20_000 {
        controller.collect_remote_work();
        if !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }

    assert!(
        cancellation_observed.load(Ordering::Relaxed),
        "replacing A with B must cancel the active selected file preview"
    );
    assert_eq!(preview_polls.load(Ordering::Relaxed), 1);
    assert!(!controller.is_polling());
    assert_eq!(controller.pending_remote_interests.len(), 4);
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [SyncInterest::OneDriveFilePreview { item }] if item == &item_a
    ));
}

#[test]
fn latest_selected_channel_files_cancels_only_the_obsolete_file_poll() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let channel_files_polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller = WorkspaceTransportController::with_remote_transport(Box::new(
        ChannelSelectionCancellationTransport {
            polls: Arc::clone(&polls),
            channel_message_polls: Arc::new(AtomicUsize::new(0)),
            channel_files_polls: Arc::clone(&channel_files_polls),
            channel_reply_polls: Arc::new(AtomicUsize::new(0)),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        },
    ));
    controller
        .synchronize_profile("remote-profile")
        .expect("identity worker must start");
    for _ in 0..10_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("remote bootstrap must succeed");
        if !controller.is_polling()
            && polls
                .lock()
                .expect("test poll list must not be poisoned")
                .len()
                == 5
        {
            break;
        }
        thread::yield_now();
    }
    let _ = controller.take_deltas();

    let team_a = remote_graph_target("remote-team-a");
    let channel_a = remote_graph_target("remote-channel-a");
    let team_b = remote_graph_target("remote-team-b");
    let channel_b = remote_graph_target("remote-channel-b");
    controller.request_remote_channel_files(team_a.clone(), channel_a.clone());
    controller
        .synchronize_profile("remote-profile")
        .expect("selected channel-files worker must start");
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );

    controller.request_remote_channel_files(team_b.clone(), channel_b.clone());
    controller.request_remote_channel_files(team_b.clone(), channel_b.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelFiles { team, channel })
            if team == &team_b && channel == &channel_b
    ));

    for _ in 0..20_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("replacement selected channel-files read must succeed");
        if !controller.is_polling() && channel_files_polls.load(Ordering::Relaxed) == 2 {
            break;
        }
        thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(channel_files_polls.load(Ordering::Relaxed), 2);
    assert!(controller.pending_remote_interests.is_empty());
    assert!(controller.is_microsoft365_active());
    let deltas = controller.take_deltas();
    assert_eq!(deltas.len(), 1);
    assert!(matches!(
        deltas[0].completed.as_slice(),
        [SyncInterest::ChannelFiles { team, channel }]
            if team == &team_b && channel == &channel_b
    ));
    let polls = polls.lock().expect("test poll list must not be poisoned");
    assert!(matches!(
        polls.as_slice(),
        [
            SyncInterest::Identity,
            SyncInterest::Chats,
            SyncInterest::Teams,
            SyncInterest::Calendar,
            SyncInterest::Files,
            SyncInterest::ChannelFiles { team, channel },
            SyncInterest::ChannelFiles { team: final_team, channel: final_channel },
        ] if team == &team_a
            && channel == &channel_a
            && final_team == &team_b
            && final_channel == &channel_b
    ));
}
