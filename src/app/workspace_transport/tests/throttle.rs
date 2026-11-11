use super::super::WorkspaceTransportController;
use super::super::polling::{POLL_THROTTLE_FALLBACK, poll_throttle_retry_delay};
use super::super::remote_worker::{InterestKind, RemoteWork, TransportRuntime};
use super::{SequencedRemoteTransport, remote_graph_target};
use crate::integrations::{RemoteSearchQuery, SyncInterest, TransportError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
fn throttle_defers_only_that_interest_kind_until_its_deadline() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
        }));
    let now = Instant::now();
    let throttled_team = remote_graph_target("remote-team-throttled");
    let throttled_channel = remote_graph_target("remote-channel-throttled");
    let other_team = remote_graph_target("remote-team-other");
    let other_channel = remote_graph_target("remote-channel-other");
    let other_post = remote_graph_target("remote-post-other");

    assert_eq!(
        controller.handle_completed_poll_at(
            SyncInterest::ChannelFiles {
                team: throttled_team,
                channel: throttled_channel,
            },
            Err(TransportError::Throttled {
                retry_after_seconds: Some(17),
            }),
            now,
        ),
        Err(TransportError::Throttled {
            retry_after_seconds: Some(17),
        })
    );

    // A user request made during the cooldown is queued, never dropped, and
    // so are reads of other kinds.
    controller.request_remote_channel_files(other_team.clone(), other_channel.clone());
    controller.request_remote_channel_messages(other_team.clone(), other_channel.clone());
    controller.request_remote_channel_replies(
        other_team.clone(),
        other_channel.clone(),
        other_post.clone(),
    );
    assert_eq!(controller.pending_remote_interests.len(), 3);

    let inside = now + Duration::from_secs(16);
    assert!(controller.poll_cooldown_blocks_at(InterestKind::ChannelFiles, inside));
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::ChannelMessages, inside));
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::Chats, inside));
    assert_eq!(
        controller.next_poll_cooldown_delay_at(inside),
        Some(Duration::from_secs(1))
    );

    // The deferred files read is skipped; the next eligible read starts.
    controller
        .start_next_remote_poll_at(inside)
        .expect("an eligible read must start");
    assert!(matches!(
        &controller.runtime,
        TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            ..
        } if matches!(active.as_interest(), SyncInterest::ChannelMessages { .. })
    ));
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(poll.as_interest(), SyncInterest::ChannelFiles { .. }))
    );

    // After the deadline the deferred read is eligible and no delay remains.
    let after = now + Duration::from_secs(17);
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::ChannelFiles, after));
    assert_eq!(controller.next_poll_cooldown_delay_at(after), None);
}

#[test]
fn every_non_identity_kind_gets_a_cooldown_and_identity_is_exempt() {
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
        }));
    let now = Instant::now();
    let chat = remote_graph_target("remote-chat-cooldown");
    for interest in [
        SyncInterest::Chats,
        SyncInterest::Teams,
        SyncInterest::ChatMessages { chat: chat.clone() },
        SyncInterest::ChatPinnedMessages { chat: chat.clone() },
        SyncInterest::ChatMembers { chat: chat.clone() },
        SyncInterest::ChatMemberPresence { chat: chat.clone() },
        SyncInterest::MessageSearch {
            query: RemoteSearchQuery::new("budget").unwrap(),
            page: 0,
        },
        SyncInterest::OneDriveFilePreview { item: chat.clone() },
        SyncInterest::Calendar,
        SyncInterest::Files,
    ] {
        let kind = InterestKind::from(&interest);
        let _ = controller.handle_completed_poll_at(
            interest,
            Err(TransportError::Throttled {
                retry_after_seconds: Some(5),
            }),
            now,
        );
        assert!(controller.poll_cooldown_blocks_at(kind, now), "{kind:?}");
        assert!(!controller.poll_cooldown_blocks_at(kind, now + Duration::from_secs(5)));
    }
    let _ = controller.handle_completed_poll_at(
        SyncInterest::Identity,
        Err(TransportError::Throttled {
            retry_after_seconds: Some(5),
        }),
        now,
    );
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::Identity, now));
}

#[test]
fn throttle_uses_a_bounded_fallback_and_resets_with_the_profile() {
    assert_eq!(poll_throttle_retry_delay(None), POLL_THROTTLE_FALLBACK);
    assert_eq!(poll_throttle_retry_delay(Some(9)), Duration::from_secs(9));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
        }));
    controller.initialized_profile = Some("old-profile".into());
    let now = Instant::now();
    let team = remote_graph_target("remote-team-fallback");
    let channel = remote_graph_target("remote-channel-fallback");

    assert_eq!(
        controller.handle_completed_poll_at(
            SyncInterest::ChannelFiles {
                team: team.clone(),
                channel: channel.clone(),
            },
            Err(TransportError::Throttled {
                retry_after_seconds: None,
            }),
            now,
        ),
        Err(TransportError::Throttled {
            retry_after_seconds: None,
        })
    );
    controller.request_remote_channel_files(team.clone(), channel.clone());
    assert_eq!(controller.pending_remote_interests.len(), 1);
    assert!(
        controller
            .poll_cooldown_blocks_at(InterestKind::ChannelFiles, now + Duration::from_secs(1))
    );

    controller.begin_profile_transition("new-profile");
    assert!(controller.poll_cooldowns.is_empty());
    controller.request_remote_channel_files(team.clone(), channel.clone());
    assert!(
        controller
            .pending_remote_interests
            .iter()
            .any(|poll| matches!(
                poll.as_interest(),
                SyncInterest::ChannelFiles {
                    team: queued_team,
                    channel: queued_channel,
                } if queued_team == &team && queued_channel == &channel
            ))
    );
}

fn presence_controller() -> WorkspaceTransportController {
    WorkspaceTransportController::with_remote_transport(Box::new(SequencedRemoteTransport {
        polls: Arc::new(Mutex::new(Vec::new())),
    }))
}

#[test]
fn presence_cooldown_does_not_block_chat_members() {
    let mut controller = presence_controller();
    let now = Instant::now();
    let chat = remote_graph_target("remote-chat-presence-cooldown");
    let _ = controller.handle_completed_poll_at(
        SyncInterest::ChatMemberPresence { chat },
        Err(TransportError::Throttled {
            retry_after_seconds: Some(30),
        }),
        now,
    );
    assert!(controller.poll_cooldown_blocks_at(InterestKind::ChatMemberPresence, now));
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::ChatMembers, now));
}

#[test]
fn a_failed_presence_refresh_hands_the_feature_a_label_keeping_marker() {
    let mut controller = presence_controller();
    let chat = remote_graph_target("remote-chat-presence-failed");
    let _ = controller.handle_completed_poll_at(
        SyncInterest::ChatMemberPresence { chat: chat.clone() },
        Err(TransportError::Unavailable("down".into())),
        Instant::now(),
    );
    let marker = controller
        .pending_deltas
        .pop_front()
        .and_then(|delta| delta.remote_chat_members)
        .expect("a presence marker");
    assert!(marker.is_presence_update());
    assert!(marker.presence_updates().is_empty());
    assert_eq!(marker.chat(), chat);
}

#[test]
fn closing_people_cancels_only_the_presence_poll() {
    let mut controller = presence_controller();
    let chat = remote_graph_target("remote-chat-presence-cancel");
    controller.request_remote_chat_members(chat.clone());
    controller.request_remote_chat_member_presence(chat.clone());
    controller.request_remote_chat_member_presence(chat.clone());
    let queued = |controller: &WorkspaceTransportController| {
        controller
            .pending_remote_interests
            .iter()
            .map(|poll| InterestKind::from(poll.as_interest()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        queued(&controller),
        [InterestKind::ChatMembers, InterestKind::ChatMemberPresence]
    );
    controller.cancel_remote_chat_member_presence();
    assert_eq!(queued(&controller), [InterestKind::ChatMembers]);
    controller.request_remote_chat_member_presence(chat);
    controller.cancel_remote_chat_members();
    assert_eq!(queued(&controller), [InterestKind::ChatMemberPresence]);
}

fn search_query(text: &str) -> RemoteSearchQuery {
    RemoteSearchQuery::new(text).expect("a valid query")
}

fn queued_kinds(controller: &WorkspaceTransportController) -> Vec<InterestKind> {
    controller
        .pending_remote_interests
        .iter()
        .map(|poll| InterestKind::from(poll.as_interest()))
        .collect()
}

#[test]
fn the_search_cooldown_does_not_block_chat_history() {
    let mut controller = presence_controller();
    let now = Instant::now();
    let _ = controller.handle_completed_poll_at(
        SyncInterest::MessageSearch {
            query: search_query("budget"),
            page: 0,
        },
        Err(TransportError::Throttled {
            retry_after_seconds: Some(30),
        }),
        now,
    );
    assert!(controller.poll_cooldown_blocks_at(InterestKind::MessageSearch, now));
    assert!(!controller.poll_cooldown_blocks_at(InterestKind::ChatMessages, now));
}

#[test]
fn a_failed_search_hands_the_feature_a_hit_free_marker_for_that_query_and_page() {
    let mut controller = presence_controller();
    let _ = controller.handle_completed_poll_at(
        SyncInterest::MessageSearch {
            query: search_query("budget"),
            page: 2,
        },
        Err(TransportError::Unavailable("down".into())),
        Instant::now(),
    );
    let marker = controller
        .pending_deltas
        .pop_front()
        .and_then(|delta| delta.remote_message_search)
        .expect("a search marker");
    assert_eq!(marker.query(), &search_query("budget"));
    assert_eq!(marker.page(), 2);
    assert!(marker.hits().is_empty());
}

#[test]
fn a_newer_search_replaces_a_queued_one_and_cancel_drops_only_searches() {
    let mut controller = presence_controller();
    let chat = remote_graph_target("remote-chat-search-cancel");
    controller.request_remote_chat_members(chat);
    controller.request_remote_message_search(search_query("one"), 0);
    controller.request_remote_message_search(search_query("two"), 0);
    controller.request_remote_message_search(search_query("two"), 0);
    assert_eq!(
        queued_kinds(&controller),
        [InterestKind::ChatMembers, InterestKind::MessageSearch]
    );
    assert!(controller.pending_remote_interests.iter().any(|poll| {
        poll.as_interest()
            == &SyncInterest::MessageSearch {
                query: search_query("two"),
                page: 0,
            }
    }));
    controller.cancel_remote_message_search();
    assert_eq!(queued_kinds(&controller), [InterestKind::ChatMembers]);
}
