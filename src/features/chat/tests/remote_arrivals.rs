//! New-message detection, in-app toasts, mute digests and the badge count.

use super::super::prefs::{ChatPrefs, LocalChatPrefsStore};
use super::*;
use crate::integrations::{RemoteChatMessage, RemoteChatMessagesProjection};
use crate::testing::TemporaryDirectory;

const A: &str = "19:chat-a-secret-id@thread.v2";
const B: &str = "19:chat-b-secret-id@thread.v2";

fn msg(id: &str, at: &str, own: bool, deleted: bool) -> RemoteChatMessage {
    RemoteChatMessage::for_test_with_reactions(
        id,
        "Ada Lovelace",
        at,
        "preview text with @Ada in plain text",
        own,
        deleted,
        Vec::new(),
    )
}

fn conversation(chat: &str, message: RemoteChatMessage) -> RemoteChatConversation {
    RemoteChatConversation::for_test(
        chat,
        RemoteChatKind::OneOnOne,
        "Design sync",
        "Topic",
        "DS",
        true,
        vec![message],
    )
}

fn dir(items: Vec<(&str, RemoteChatMessage)>) -> RemoteChatProjection {
    RemoteChatProjection::for_test(
        items
            .into_iter()
            .map(|(chat, message)| conversation(chat, message))
            .collect(),
    )
}

fn seeded(first_at: &str) -> ChatState {
    let mut chat = ChatState::default();
    chat.set_remote_account("acct-1");
    chat.apply_remote_projection(dir(vec![(A, msg("a1", first_at, false, false))]));
    assert!(
        chat.take_notification_toast().is_none(),
        "seeding is silent"
    );
    chat
}

#[test]
fn the_first_projection_seeds_silently_and_a_later_message_announces_once() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(dir(vec![
        (A, msg("a1", "2026-11-06T10:00:00Z", false, false)),
        (B, msg("b1", "2026-11-06T10:05:00Z", false, false)),
    ]));
    assert!(chat.take_notification_toast().is_none());

    chat.apply_remote_projection(dir(vec![
        (A, msg("a2", "2026-11-06T10:10:00Z", false, false)),
        (B, msg("b1", "2026-11-06T10:05:00Z", false, false)),
    ]));
    assert_eq!(
        chat.take_notification_toast().as_deref(),
        Some("New Microsoft 365 message")
    );
    assert!(chat.take_notification_toast().is_none(), "announced once");

    // The same projection again, and an unchanged newest message, are quiet.
    chat.apply_remote_projection(dir(vec![
        (A, msg("a2", "2026-11-06T10:10:00Z", false, false)),
        (B, msg("b1", "2026-11-06T10:05:00Z", false, false)),
    ]));
    assert!(chat.take_notification_toast().is_none());
}

#[test]
fn a_different_message_that_is_not_strictly_later_never_announces() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    // Different target, same time: not strictly later.
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a-other", "2026-11-06T10:00:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_none());
    // Different target, earlier time (for example a deleted newest row).
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a-old", "2026-11-06T09:00:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_none());
    // An unparsable time can never prove anything newer.
    chat.apply_remote_projection(dir(vec![(A, msg("a-x", "Unknown time", false, false))]));
    assert!(chat.take_notification_toast().is_none());
    // A genuinely later one still announces after all that.
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a3", "2026-11-06T11:00:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_some());
}

#[test]
fn own_and_deleted_messages_do_not_announce_but_move_the_watermark() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("own", "2026-11-06T10:10:00Z", true, false),
    )]));
    assert!(chat.take_notification_toast().is_none());
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("gone", "2026-11-06T10:20:00Z", false, true),
    )]));
    assert!(chat.take_notification_toast().is_none());
    // The own message cannot announce itself later either.
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("own", "2026-11-06T10:10:00Z", true, false),
    )]));
    assert!(chat.take_notification_toast().is_none());
}

#[test]
fn the_open_chat_announces_only_while_the_window_is_unfocused() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert!(
        chat.take_notification_toast().is_none(),
        "selected and focused: already in view"
    );
    assert_eq!(chat.remote_chats.get(0).expect("chat").new_count(), 0);

    chat.set_window_focused(false);
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a3", "2026-11-06T10:20:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_some());
    assert_eq!(chat.remote_chats.get(0).expect("chat").new_count(), 1);
}

#[test]
fn a_chat_that_appears_later_announces_only_if_newer_than_the_session_start() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    // An old chat surfacing in the bounded list is not news.
    chat.apply_remote_projection(dir(vec![
        (A, msg("a1", "2026-11-06T10:00:00Z", false, false)),
        (B, msg("b-old", "2026-11-06T08:00:00Z", false, false)),
    ]));
    assert!(chat.take_notification_toast().is_none());
    // A brand-new chat with a fresh message is.
    chat.apply_remote_projection(dir(vec![
        (A, msg("a1", "2026-11-06T10:00:00Z", false, false)),
        (B, msg("b-old", "2026-11-06T08:00:00Z", false, false)),
        (
            "19:chat-c@thread.v2",
            msg("c1", "2026-11-06T10:30:00Z", false, false),
        ),
    ]));
    assert!(chat.take_notification_toast().is_some());
}

#[test]
fn several_chats_make_one_generic_toast_with_totals() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(dir(vec![
        (A, msg("a1", "2026-11-06T10:00:00Z", false, false)),
        (B, msg("b1", "2026-11-06T10:00:00Z", false, false)),
    ]));
    let _ = chat.take_notification_toast();
    chat.apply_remote_projection(dir(vec![
        (A, msg("a2", "2026-11-06T10:10:00Z", false, false)),
        (B, msg("b2", "2026-11-06T10:11:00Z", false, false)),
    ]));
    assert_eq!(
        chat.take_notification_toast().as_deref(),
        Some("2 new Microsoft 365 messages in 2 chats")
    );
}

#[test]
fn a_mention_label_comes_only_from_structured_history() {
    // Plain "@Ada" text in the preview never labels a mention.
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert_eq!(
        chat.take_notification_toast().as_deref(),
        Some("New Microsoft 365 message")
    );

    let history = |mention_on_newest: bool| {
        RemoteChatMessagesProjection::for_test(
            A,
            vec![
                RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "a2",
                    "Ada",
                    "2026-11-06T10:10:00Z",
                    "older",
                    !mention_on_newest,
                ),
                RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "a3",
                    "Ada",
                    "2026-11-06T10:20:00Z",
                    "newest",
                    mention_on_newest,
                ),
            ],
        )
    };
    for (mention_on_newest, expected) in [
        (true, "You were mentioned in a Microsoft 365 chat"),
        (false, "New Microsoft 365 message"),
    ] {
        let mut chat = seeded("2026-11-06T10:00:00Z");
        chat.select_remote_conversation(0);
        let _ = chat.take_remote_chat_messages_request();
        assert!(chat.apply_remote_chat_messages_projection(history(mention_on_newest)));
        chat.set_window_focused(false);
        chat.apply_remote_projection(dir(vec![(
            A,
            msg("a3", "2026-11-06T10:20:00Z", false, false),
        )]));
        assert_eq!(chat.take_notification_toast().as_deref(), Some(expected));
    }
}

#[test]
fn chat_names_are_opt_in() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.set_show_chat_name_in_notifications(true);
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert_eq!(
        chat.take_notification_toast().as_deref(),
        Some("New message in Design sync")
    );
}

#[test]
fn disabled_notifications_drain_without_building_a_backlog() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.set_notifications_enabled(false);
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_none());
    chat.set_notifications_enabled(true);
    assert!(chat.take_notification_toast().is_none(), "nothing replays");
}

#[test]
fn a_muted_chat_is_silent_and_unmuting_restores_announcements() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    assert!(!chat.is_remote_chat_muted(0));
    chat.set_remote_chat_muted(0, true);
    assert!(chat.is_remote_chat_muted(0));
    assert!(chat.is_prefs_dirty());

    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_none());

    chat.set_remote_chat_muted(0, false);
    assert!(!chat.is_remote_chat_muted(0));
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a3", "2026-11-06T10:20:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_some());
}

#[test]
fn muting_needs_a_verified_account_and_never_stores_the_chat_id() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a1", "2026-11-06T10:00:00Z", false, false),
    )]));
    assert!(!chat.can_mute_remote_chat(0), "no account yet");
    chat.set_remote_chat_muted(0, true);
    assert!(!chat.is_remote_chat_muted(0));

    chat.set_remote_account("acct-1");
    assert!(chat.can_mute_remote_chat(0));
    chat.set_remote_chat_muted(0, true);

    let directory = TemporaryDirectory::new("arrivals-prefs");
    let store = LocalChatPrefsStore::at(directory.file("profile", "chat-prefs-v1.json"));
    store.save(&chat.prefs()).expect("saves");
    let text = std::fs::read_to_string(store.path()).expect("file");
    for secret in [A, "chat-a-secret-id", "acct-1", "Design sync"] {
        assert!(!text.contains(secret), "the file must not contain {secret}");
    }
    let mut restored = ChatState::default();
    restored.restore_prefs(store.load().expect("loads").expect("present"));
    restored.set_remote_account("acct-1");
    restored.apply_remote_projection(dir(vec![(
        A,
        msg("a1", "2026-11-06T10:00:00Z", false, false),
    )]));
    assert!(
        restored.is_remote_chat_muted(0),
        "the digest survives a restart"
    );
    restored.set_remote_account("someone-else");
    assert!(
        !restored.is_remote_chat_muted(0),
        "a digest is keyed to the account"
    );
}

#[test]
fn the_badge_counts_announced_arrivals_and_resets_when_read() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    chat.select_workspace_conversation(0);
    for (index, at) in ["10:10", "10:20", "10:30"].iter().enumerate() {
        chat.apply_remote_projection(dir(vec![(
            A,
            msg(
                &format!("a{}", index + 2),
                &format!("2026-11-06T{at}:00Z"),
                false,
                false,
            ),
        )]));
    }
    assert_eq!(chat.remote_chats.get(0).expect("chat").new_count(), 3);
    assert!(chat.remote_chats.get(0).expect("chat").is_unread());
    chat.select_remote_conversation(0);
    assert_eq!(chat.remote_chats.get(0).expect("chat").new_count(), 0);
}

#[test]
fn arrivals_are_session_only_and_a_new_session_seeds_silently_again() {
    let mut chat = seeded("2026-11-06T10:00:00Z");
    let local = chat.stored();
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a2", "2026-11-06T10:10:00Z", false, false),
    )]));
    assert!(chat.take_notification_toast().is_some());
    assert_eq!(chat.stored(), local, "nothing remote is persisted");
    assert!(!chat.is_persistence_dirty());

    chat.clear_remote_projection();
    chat.apply_remote_projection(dir(vec![(
        A,
        msg("a9", "2026-11-06T12:00:00Z", false, false),
    )]));
    assert!(
        chat.take_notification_toast().is_none(),
        "a reconnect re-seeds instead of announcing the backlog"
    );
}

#[test]
fn the_prefs_default_keeps_in_app_notifications_on_and_names_off() {
    let prefs = ChatPrefs::default();
    assert!(prefs.notifications_enabled && !prefs.show_chat_name);
}

#[test]
fn system_notifications_are_off_by_default_and_need_every_condition() {
    let mut chat = ChatState::default();
    assert!(!chat.os_notifications_enabled(), "opt-in");
    assert!(
        !chat.os_notifications_available(),
        "no notifier until the app says so"
    );
    assert!(!chat.os_notifications_wanted());

    chat.set_os_notifications_available(true);
    assert!(!chat.os_notifications_wanted(), "available but not chosen");

    chat.set_os_notifications_enabled(true);
    assert!(chat.os_notifications_wanted());
    assert!(chat.is_prefs_dirty(), "the choice is persisted");

    chat.set_notifications_enabled(false);
    assert!(
        !chat.os_notifications_wanted(),
        "in-app notifications gate it"
    );
    chat.set_notifications_enabled(true);
    chat.set_os_notifications_available(false);
    assert!(!chat.os_notifications_wanted(), "no truthful notifier");
}

#[test]
fn the_system_choice_survives_a_prefs_round_trip_but_availability_does_not() {
    let mut chat = ChatState::default();
    chat.set_os_notifications_available(true);
    chat.set_os_notifications_enabled(true);
    let directory = TemporaryDirectory::new("os-prefs");
    let store = LocalChatPrefsStore::at(directory.file("profile", "chat-prefs-v1.json"));
    store.save(&chat.prefs()).expect("saves");

    let mut restored = ChatState::default();
    restored.restore_prefs(store.load().expect("loads").expect("present"));
    assert!(restored.os_notifications_enabled());
    assert!(
        !restored.os_notifications_wanted(),
        "availability is a property of this machine, never stored"
    );
}
