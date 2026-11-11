use super::super::WorkspaceTransportController;
use super::super::remote_worker::RemotePoll;
use super::{
    BackgroundTransport, FailingTransport, RecordingTransport, SequencedRemoteTransport,
    remote_graph_target,
};
use crate::integrations::{
    DelegatedTokens, ProviderResourceRef, RemoteChatMessageImportance, SyncInterest,
    TransportError, WorkspaceCommand, WorkspaceDelta,
};
use std::sync::{Arc, Mutex, atomic::AtomicUsize};
use std::thread;

#[test]
fn local_preview_controller_syncs_profiles_and_stages_commands() {
    let mut controller = WorkspaceTransportController::default();

    controller
        .synchronize_profile("local-preview")
        .expect("the local adapter must synchronize");
    assert_eq!(controller.status_label(), "Local preview");

    let receipt = controller
        .dispatch_now(WorkspaceCommand::SendChannelPost {
            team: ProviderResourceRef::local_preview("Bike Commuters"),
            channel: ProviderResourceRef::local_preview("Bike Commuters · General"),
            body: "A local staged post".into(),
        })
        .expect("the local adapter must stage an accepted post");
    assert_eq!(receipt.command_id, "local:1");
    assert_eq!(
        WorkspaceTransportController::receipt_summary(&receipt),
        "Staged in the local preview"
    );

    controller.reset();
    assert_eq!(controller.status_label(), "Local preview");
}

#[test]
fn entra_handoff_replaces_runtime_and_discards_local_fifo_without_exposing_a_token() {
    let token_marker = "handoff-token-must-not-appear-in-app-state";
    let mut controller = WorkspaceTransportController::default();
    controller
        .synchronize_profile("local-profile")
        .expect("the local adapter must synchronize");
    controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("pre-connect-chat"),
        body: "Do not replay this for a newly connected account".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }]);
    controller.pending_notice = Some("stale local notice".into());
    controller.pending_error = Some(TransportError::Unavailable("stale local error".into()));
    controller
        .pending_remote_interests
        .push_back(RemotePoll::interest(SyncInterest::Chats));
    controller
        .pending_deltas
        .push_back(WorkspaceDelta::default());

    controller
        .connect_microsoft365(DelegatedTokens::for_test(token_marker))
        .expect("an in-memory delegated token must connect the Graph transport");

    assert!(controller.is_microsoft365_active());
    assert!(!controller.is_polling());
    assert_eq!(controller.status_label(), "Connecting to Microsoft 365");
    assert!(!controller.status_label().contains(token_marker));
    assert!(controller.initialized_profile.is_none());
    assert!(controller.take_notice().is_none());
    assert!(controller.pending_error.is_none());
    assert!(controller.pending_remote_interests.is_empty());
    assert!(controller.take_deltas().is_empty());
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        !controller.take_remote_projection_clear_for_transport_loss_requested(),
        "an account connection must not preserve post-recovery state"
    );
    assert!(controller.dispatch_due().is_empty());
}

#[test]
fn rejected_entra_handoff_keeps_the_current_runtime_and_never_echoes_the_token() {
    let token_marker = "invalid-token-must-not-appear\n";
    let mut controller = WorkspaceTransportController::default();
    controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("still-local-chat"),
        body: "Retain this command when handoff construction fails".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }]);

    let error = controller
        .connect_microsoft365(DelegatedTokens::for_test(token_marker))
        .expect_err("a control character must reject the transport token");

    assert!(!error.to_string().contains(token_marker));
    assert!(!controller.is_microsoft365_active());
    assert_eq!(controller.status_label(), "Local preview");
    assert_eq!(controller.pending_command_count(), 1);
}

#[test]
fn disconnect_forces_a_fresh_local_preview_and_drops_its_fifo() {
    let mut controller = WorkspaceTransportController::default();
    controller
        .connect_microsoft365(DelegatedTokens::for_test("disconnect-test-token"))
        .expect("a delegated token must create the remote runtime");
    controller.enqueue([WorkspaceCommand::SendChannelPost {
        team: ProviderResourceRef::local_preview("team"),
        channel: ProviderResourceRef::local_preview("channel"),
        body: "Do not carry this across disconnect".into(),
    }]);
    controller.initialized_profile = Some("remote-profile".into());
    controller.pending_notice = Some("stale remote notice".into());
    controller.pending_error = Some(TransportError::Unavailable("stale remote error".into()));
    controller
        .pending_remote_interests
        .push_back(RemotePoll::interest(SyncInterest::Chats));
    controller
        .pending_deltas
        .push_back(WorkspaceDelta::default());

    controller.disconnect_microsoft365();

    assert!(!controller.is_microsoft365_active());
    assert!(!controller.is_polling());
    assert_eq!(controller.status_label(), "Local preview");
    assert!(controller.initialized_profile.is_none());
    assert!(controller.take_notice().is_none());
    assert!(controller.pending_error.is_none());
    assert!(controller.pending_remote_interests.is_empty());
    assert!(controller.take_deltas().is_empty());
    assert_eq!(controller.pending_command_count(), 0);
    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        !controller.take_remote_projection_clear_for_transport_loss_requested(),
        "an explicit disconnect must discard post-recovery state"
    );
    assert!(controller.dispatch_due().is_empty());
}

#[test]
fn failed_local_profile_sync_is_reported_once_per_session() {
    let polls = Arc::new(AtomicUsize::new(0));
    let mut controller =
        WorkspaceTransportController::with_local_transport(Box::new(FailingTransport {
            polls: Arc::clone(&polls),
        }));

    assert!(controller.synchronize_profile("profile").is_err());
    assert!(controller.synchronize_profile("profile").is_ok());
    assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(controller.status_label(), "Disconnected");
}

#[test]
fn configured_local_transport_receives_dispatched_commands() {
    let dispatches = Arc::new(AtomicUsize::new(0));
    let mut controller =
        WorkspaceTransportController::with_local_transport(Box::new(RecordingTransport {
            dispatches: Arc::clone(&dispatches),
        }));

    let receipt = controller
        .dispatch_now(WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::local_preview("chat-42"),
            body: "A configured local adapter must receive this".into(),
            importance: RemoteChatMessageImportance::Normal,
            attachment: None,
        })
        .expect("configured local adapter must handle dispatches");

    assert_eq!(receipt.command_id, "recording:1");
    assert_eq!(dispatches.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn remote_identity_poll_runs_outside_the_update_path() {
    let polls = Arc::new(AtomicUsize::new(0));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(BackgroundTransport {
            polls: Arc::clone(&polls),
        }));

    controller
        .synchronize_profile("profile")
        .expect("starting a background synchronization must succeed");
    assert!(controller.is_polling());

    for _ in 0..10_000 {
        let _ = controller.synchronize_profile("profile");
        if !controller.is_polling() {
            break;
        }
        std::thread::yield_now();
    }

    assert!(!controller.is_polling());
    assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 1);

    let receipt = controller
        .dispatch_now(WorkspaceCommand::SendChannelPost {
            team: ProviderResourceRef::local_preview("team"),
            channel: ProviderResourceRef::local_preview("channel"),
            body: "Retain local behavior while remote chat delivery is unavailable".into(),
        })
        .expect("unsupported remote commands must remain locally staged");
    assert_eq!(receipt.command_id, "local:1");
}

#[test]
fn remote_sync_orders_identity_before_bounded_projections_and_drains_complete_deltas() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::clone(&polls),
        }));

    controller
        .synchronize_profile("local-profile")
        .expect("identity worker must start");
    assert!(controller.is_polling());

    for _ in 0..10_000 {
        controller
            .synchronize_profile("local-profile")
            .expect("sequenced remote sync must not fail");
        if !controller.is_polling()
            && polls
                .lock()
                .expect("test poll list must not be poisoned")
                .len()
                == 5
        {
            break;
        }
        std::thread::yield_now();
    }

    assert_eq!(
        polls
            .lock()
            .expect("test poll list must not be poisoned")
            .as_slice(),
        [
            SyncInterest::Identity,
            SyncInterest::Chats,
            SyncInterest::Teams,
            SyncInterest::Calendar,
            SyncInterest::Files,
        ]
    );
    let deltas = controller.take_deltas();
    assert_eq!(deltas.len(), 5);
    assert_eq!(deltas[0].completed, [SyncInterest::Identity]);
    assert!(deltas[0].identity.is_some());
    assert_eq!(deltas[1].completed, [SyncInterest::Chats]);
    assert!(deltas[1].remote_chats.is_some());
    assert_eq!(deltas[2].completed, [SyncInterest::Teams]);
    assert!(deltas[2].remote_teams.is_some());
    assert_eq!(deltas[3].completed, [SyncInterest::Calendar]);
    assert!(deltas[3].remote_calendar.is_some());
    assert_eq!(deltas[4].completed, [SyncInterest::Files]);
    assert!(controller.take_deltas().is_empty());
}

#[test]
fn selected_channel_messages_replace_stale_queued_selection_after_bootstrap() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::clone(&polls),
        }));
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
    controller.request_remote_channel_messages(team_a, channel_a);
    controller.request_remote_channel_messages(
        remote_graph_target("remote-team-a"),
        remote_graph_target("remote-channel-a"),
    );
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

    for _ in 0..10_000 {
        controller
            .synchronize_profile("remote-profile")
            .expect("selected channel read must succeed");
        if !controller.is_polling()
            && polls
                .lock()
                .expect("test poll list must not be poisoned")
                .len()
                == 6
        {
            break;
        }
        thread::yield_now();
    }

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
        ] if team == &team_b && channel == &channel_b
    ));
}

#[test]
fn channel_files_requests_deduplicate_without_cross_wiring_other_channel_reads() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
        }));
    let team_a = remote_graph_target("remote-team-a");
    let channel_a = remote_graph_target("remote-channel-a");
    let post_a = remote_graph_target("remote-post-a");
    let team_b = remote_graph_target("remote-team-b");
    let channel_b = remote_graph_target("remote-channel-b");

    controller.request_remote_channel_messages(team_a.clone(), channel_a.clone());
    controller.request_remote_channel_replies(team_a.clone(), channel_a.clone(), post_a.clone());
    controller.request_remote_channel_files(team_a, channel_a);
    controller.request_remote_channel_files(
        remote_graph_target("remote-team-a"),
        remote_graph_target("remote-channel-a"),
    );
    controller.request_remote_channel_files(team_b.clone(), channel_b.clone());

    assert_eq!(controller.pending_remote_interests.len(), 3);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelMessages { .. })
    ));
    assert!(matches!(
        controller
            .pending_remote_interests
            .get(1)
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelReplies { team, channel, post })
            if team == &remote_graph_target("remote-team-a")
                && channel == &remote_graph_target("remote-channel-a")
                && post == &post_a
    ));
    assert!(matches!(
        controller
            .pending_remote_interests
            .get(2)
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelFiles { team, channel })
            if team == &team_b && channel == &channel_b
    ));

    controller.cancel_remote_channel_files();

    assert_eq!(controller.pending_remote_interests.len(), 2);
    assert!(matches!(
        controller
            .pending_remote_interests
            .front()
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelMessages { .. })
    ));
    assert!(matches!(
        controller
            .pending_remote_interests
            .get(1)
            .map(RemotePoll::as_interest),
        Some(SyncInterest::ChannelReplies { .. })
    ));
}
