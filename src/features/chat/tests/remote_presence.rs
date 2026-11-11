use super::super::ChatState;
use super::super::remote::{PRESENCE_REFRESH_INTERVAL, PresenceSchedule, RemoteChatMembersPanel};
use crate::integrations::{
    PresenceStatus, RemoteChatConversation, RemoteChatKind, RemoteChatMember,
    RemoteChatMembersProjection, RemoteChatProjection, RemoteMemberPresence,
};
use std::time::Instant;

const CHAT: &str = "19:presence@thread.v2";

fn open_chat() -> ChatState {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            CHAT,
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Hi",
            "AL",
            false,
            Vec::new(),
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    chat
}

fn roster() -> RemoteChatMembersProjection {
    RemoteChatMembersProjection::for_test(
        CHAT,
        vec![
            RemoteChatMember::for_test("Ada", None)
                .with_presence_for_test(RemoteMemberPresence::Available),
            RemoteChatMember::for_test("Bob", None),
        ],
    )
}

fn labels(chat: &ChatState) -> Vec<Option<RemoteMemberPresence>> {
    match chat.remote_chats.members_panel(0) {
        Some(RemoteChatMembersPanel::Ready(rows)) => rows.iter().map(|r| r.presence).collect(),
        _ => panic!("People must be ready"),
    }
}

fn later() -> Instant {
    Instant::now() + PRESENCE_REFRESH_INTERVAL + std::time::Duration::from_secs(1)
}

#[test]
fn deadline_is_set_on_ready_and_a_request_waits_for_it() {
    let mut chat = open_chat();
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle,
        "nothing is due while People is still loading"
    );
    assert!(chat.apply_remote_chat_members_projection(roster()));
    assert_eq!(labels(&chat), [Some(RemoteMemberPresence::Available), None]);
    assert!(matches!(
        chat.remote_chat_member_presence_schedule(Instant::now(), true),
        PresenceSchedule::Wait(wait) if wait <= PRESENCE_REFRESH_INTERVAL
    ));
    assert!(matches!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Request(target) if target == roster().chat()
    ));
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle,
        "the request consumed the deadline; only an answer re-arms it"
    );
}

#[test]
fn an_unfocused_window_holds_the_refresh_back() {
    let mut chat = open_chat();
    chat.apply_remote_chat_members_projection(roster());
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), false),
        PresenceSchedule::Idle
    );
    assert!(matches!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Request(_)
    ));
}

#[test]
fn an_answer_updates_labels_and_rearms_and_a_failure_keeps_labels() {
    let mut chat = open_chat();
    chat.apply_remote_chat_members_projection(roster());
    let _ = chat.remote_chat_member_presence_schedule(later(), true);
    let update = RemoteChatMembersProjection::presence_update_for_test(
        CHAT,
        vec![
            Some(RemoteMemberPresence::Busy),
            Some(RemoteMemberPresence::Offline),
        ],
        PresenceStatus::Available,
    );
    assert!(chat.apply_remote_chat_members_projection(update));
    assert_eq!(
        labels(&chat),
        [
            Some(RemoteMemberPresence::Busy),
            Some(RemoteMemberPresence::Offline)
        ]
    );
    assert!(matches!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Request(_)
    ));
    let failed = RemoteChatMembersProjection::presence_update_for_test(
        CHAT,
        Vec::new(),
        PresenceStatus::Unavailable,
    );
    chat.apply_remote_chat_members_projection(failed);
    assert_eq!(
        labels(&chat),
        [
            Some(RemoteMemberPresence::Busy),
            Some(RemoteMemberPresence::Offline)
        ]
    );
    assert!(matches!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Request(_)
    ));
}

#[test]
fn not_granted_stops_the_schedule_and_clears_labels() {
    let mut chat = open_chat();
    chat.apply_remote_chat_members_projection(roster());
    let denied = RemoteChatMembersProjection::presence_update_for_test(
        CHAT,
        Vec::new(),
        PresenceStatus::NotGranted,
    );
    chat.apply_remote_chat_members_projection(denied);
    assert_eq!(labels(&chat), [None, None]);
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle
    );
    let mut chat = open_chat();
    chat.apply_remote_chat_members_projection(
        roster().with_status_for_test(PresenceStatus::UnsupportedInCloud),
    );
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle
    );
}

#[test]
fn closing_or_changing_selection_clears_the_deadline_and_rejects_late_updates() {
    let mut chat = open_chat();
    chat.apply_remote_chat_members_projection(roster());
    chat.toggle_remote_chat_members(0);
    assert!(chat.take_remote_chat_members_cancellation_requested());
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle
    );
    let late = RemoteChatMembersProjection::presence_update_for_test(
        CHAT,
        vec![None, None],
        PresenceStatus::Available,
    );
    assert!(!chat.apply_remote_chat_members_projection(late));
    assert_eq!(
        chat.remote_chat_member_presence_schedule(later(), true),
        PresenceSchedule::Idle,
        "a rejected late answer must not re-arm the schedule"
    );
}

#[test]
fn presence_is_never_persisted() {
    let mut chat = open_chat();
    let before = chat.stored();
    chat.apply_remote_chat_members_projection(roster());
    assert_eq!(chat.stored(), before);
    assert!(!chat.is_persistence_dirty());
}
