use super::super::{
    ChatState, ConversationSelection, LocalMessageMutationError, MessageAction, SendOutcome,
};
use super::{
    remote_history, remote_pinned_messages, remote_projection, remote_quote_source_history,
    remote_reaction_history, remote_reaction_projection,
};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMember, RemoteChatMembersProjection,
    RemoteChatMessage, RemoteChatMessageImportance, RemoteChatMessagesProjection,
    RemoteChatProjection, RemotePinnedChatMessagesProjection,
};
use crate::shared::reaction::ReactionKind;
use crate::testing::render_headlessly;
use crate::ui::toast::ToastState;

#[test]
fn remote_chat_projection_is_ephemeral_and_safe_to_clear() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    let projection = RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
        "19:remote-chat@thread.v2",
        RemoteChatKind::OneOnOne,
        "Ada Lovelace",
        "Design sync",
        "AL",
        true,
        vec![RemoteChatMessage::for_test(
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Safe plain text",
            false,
            false,
        )],
    )]);

    chat.apply_remote_projection(projection);

    assert!(!chat.remote_chats.is_empty());
    assert!(!chat.is_persistence_dirty());
    assert_eq!(chat.stored(), stored_before);

    chat.select_remote_conversation(0);
    assert_eq!(chat.selected, ConversationSelection::Remote(0));
    assert_eq!(chat.selected_messages()[0].text, "Safe plain text");
    assert!(chat.composer_for(chat.selected).is_none());
    assert!(matches!(
        chat.send(),
        SendOutcome::Rejected(LocalMessageMutationError::Missing)
    ));

    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:remote-chat@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            false,
            vec![RemoteChatMessage::for_test(
                "Ada Lovelace",
                "2026-11-06T10:31:00Z",
                "Updated remote content",
                false,
                false,
            )],
        ),
    ]));
    assert_eq!(chat.selected, ConversationSelection::Remote(0));
    assert_eq!(chat.selected_messages()[0].text, "Updated remote content");

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Heart),
        &mut toast,
    );
    assert!(chat.selected_messages()[0].reactions.is_empty());
    assert!(!chat.is_persistence_dirty());

    render_headlessly(|ui| {
        chat.draw_sidebar(ui, &mut toast);
        chat.draw_conversation(ui, &mut toast);
    });

    chat.clear_remote_projection();
    assert!(chat.remote_chats.is_empty());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert_eq!(chat.stored(), stored_before);
}

#[test]
fn selecting_remote_chat_queues_one_target_bound_history_read_without_persistence() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_projection(
        "19:history-target@thread.v2",
        "Directory preview",
    ));
    let expected_target = remote_history("19:history-target@thread.v2", "Detailed history").chat();

    chat.select_remote_conversation(0);

    assert_eq!(
        chat.take_remote_chat_messages_request(),
        Some(expected_target),
        "the selected chat target must be retained opaquely for the controller"
    );
    assert!(chat.take_remote_chat_messages_request().is_none());
    assert!(
        !chat.take_remote_chat_messages_cancellation_requested(),
        "initial selection must not cancel an unrelated remote read"
    );
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn selected_chat_history_rejects_a_late_target_after_selection_changes() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:first-history@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "First chat",
            "AL",
            false,
            vec![RemoteChatMessage::for_test(
                "Ada Lovelace",
                "2026-11-06T10:30:00Z",
                "First directory preview",
                false,
                false,
            )],
        ),
        RemoteChatConversation::for_test(
            "19:second-history@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Second chat",
            "GH",
            false,
            vec![RemoteChatMessage::for_test(
                "Grace Hopper",
                "2026-11-06T10:30:00Z",
                "Second directory preview",
                false,
                false,
            )],
        ),
    ]));
    let stale = remote_history("19:first-history@thread.v2", "Late first history");
    let current = remote_history("19:second-history@thread.v2", "Current second history");

    chat.select_remote_conversation(0);
    assert_eq!(chat.take_remote_chat_messages_request(), Some(stale.chat()));

    chat.select_remote_conversation(1);
    assert!(
        chat.take_remote_chat_messages_cancellation_requested(),
        "leaving the first remote chat must cancel only its selected-history read"
    );
    assert_eq!(
        chat.take_remote_chat_messages_request(),
        Some(current.chat())
    );
    assert!(!chat.apply_remote_chat_messages_projection(stale));
    assert_eq!(chat.selected, ConversationSelection::Remote(1));
    assert_eq!(chat.selected_messages()[0].text, "Second directory preview");

    assert!(chat.apply_remote_chat_messages_projection(current));
    assert_eq!(chat.selected_messages()[0].text, "Current second history");
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn selected_chat_history_access_denied_clears_content_without_persisting() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_projection(
        "19:denied-history@thread.v2",
        "Directory preview that must not outlive a target-bound denial",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::access_denied_for_test("19:denied-history@thread.v2")
    ));
    assert!(chat.selected_messages().is_empty());
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn directory_refresh_reloads_selected_history_before_reenabling_reactions() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:refresh-history@thread.v2",
        "Initial directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(chat.apply_remote_chat_messages_projection(remote_history(
        "19:refresh-history@thread.v2",
        "Detailed selected history",
    )));
    assert_eq!(
        chat.selected_messages()[0].text,
        "Detailed selected history"
    );

    chat.apply_remote_projection(remote_projection(
        "19:refresh-history@thread.v2",
        "Fresh directory preview",
    ));

    // The loaded rows stay visible (stale) while the reload runs, instead of
    // collapsing to the single directory preview row.
    assert_eq!(
        chat.selected_messages()[0].text,
        "Detailed selected history"
    );
    assert_eq!(
        chat.remote_chats.history_state(0),
        Some(super::super::remote::RemoteChatHistoryState::Refreshing),
        "the visible rows now require a new target-bound history before reaction controls unlock"
    );
    assert_eq!(
        chat.take_remote_chat_messages_request(),
        Some(
            RemoteChatMessagesProjection::for_test("19:refresh-history@thread.v2", Vec::new(),)
                .chat()
        ),
    );
    assert!(chat.apply_remote_chat_messages_projection(remote_history(
        "19:refresh-history@thread.v2",
        "Fresh selected history",
    )));
    assert_eq!(chat.selected_messages()[0].text, "Fresh selected history");
}

#[test]
fn directory_refresh_keeps_selected_rows_stale_and_gated_while_reloading() {
    const CHAT_ID: &str = "19:stale-keep@thread.v2";
    const MESSAGE_ID: &str = "stale-keep-message";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID, MESSAGE_ID, "Preview", 0, false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![
                RemoteChatMessage::for_test("Ada", "2026-11-06T10:30:00Z", "One", false, false),
                RemoteChatMessage::for_test_with_reactions(
                    MESSAGE_ID,
                    "Ada Lovelace",
                    "2026-11-06T10:31:00Z",
                    "Two",
                    false,
                    false,
                    Vec::new(),
                ),
            ],
        ))
    );
    assert_eq!(chat.selected_messages().len(), 2);

    // Two directory refreshes in a row (the second lands mid-reload) must
    // both keep the full loaded history.
    for _ in 0..2 {
        chat.apply_remote_projection(remote_reaction_projection(
            CHAT_ID,
            "newer-message",
            "Preview",
            0,
            false,
        ));
        assert_eq!(chat.selected_messages().len(), 2);
        assert_eq!(
            chat.remote_chats.history_state(0),
            Some(super::super::remote::RemoteChatHistoryState::Refreshing)
        );
        let view = chat
            .remote_chats
            .reaction_view(0, 1, ReactionKind::Like)
            .expect("stale row is visible");
        assert!(!view.can_react, "stale rows must not unlock reactions");
        assert!(
            chat.remote_chats
                .quote_reply_view(0, 1)
                .is_none_or(|view| !view.can_quote)
        );
    }
    assert!(chat.take_remote_chat_messages_request().is_some());
}

#[test]
fn directory_refresh_gives_unselected_chats_the_fresh_preview() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:a@thread.v2",
            RemoteChatKind::OneOnOne,
            "A",
            "a",
            "A",
            false,
            vec![RemoteChatMessage::for_test(
                "A",
                "t",
                "old preview",
                false,
                false,
            )],
        ),
        RemoteChatConversation::for_test(
            "19:b@thread.v2",
            RemoteChatKind::OneOnOne,
            "B",
            "b",
            "B",
            false,
            Vec::new(),
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_history(
            "19:a@thread.v2",
            "full history"
        ))
    );
    chat.select_remote_conversation(1);
    chat.apply_remote_projection(remote_projection("19:a@thread.v2", "fresh preview"));
    assert_eq!(
        chat.remote_chats.get(0).expect("a").messages()[0].text,
        "fresh preview"
    );
}

#[test]
fn toggling_the_only_quote_off_restores_the_priority_chosen_before_quoting() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection("19:requote@thread.v2", "Preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_quote_source_history(
            "19:requote@thread.v2",
            "quote-source",
            "Source",
        ))
    );
    chat.remote_chats
        .get_mut(0)
        .expect("row")
        .set_composer_importance(RemoteChatMessageImportance::Urgent);
    chat.toggle_remote_quote_source(0, 0).expect("first quote");
    assert_eq!(
        chat.remote_chats.get(0).expect("row").composer_importance(),
        RemoteChatMessageImportance::Normal
    );
    chat.toggle_remote_quote_source(0, 0).expect("toggle off");
    assert!(!chat.remote_chats.get(0).expect("row").has_quote_reply());
    assert_eq!(
        chat.remote_chats.get(0).expect("row").composer_importance(),
        RemoteChatMessageImportance::Urgent,
        "removing the last source must restore the pre-quote priority"
    );
}

#[test]
fn pruned_unbound_reaction_action_releases_its_gate() {
    const CHAT_ID: &str = "19:prune-gate@thread.v2";
    const MESSAGE_ID: &str = "prune-gate-message";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID, MESSAGE_ID, "Body", 0, false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, "Body", 0, false,
        ))
    );
    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert_eq!(chat.pending_actions.len(), 1);

    // A directory refresh with a newer message makes history stale before
    // the action is drained, so the click is pruned. Its gate must go too.
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID,
        "newer-message",
        "Body",
        0,
        false,
    ));
    assert!(chat.take_actions().is_empty());
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("row still visible")
            .pending,
        "a pruned click must not leave the reaction locked"
    );

    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, "Body", 0, false,
        ))
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert_eq!(chat.take_actions().len(), 1);
}

#[test]
fn unavailable_selected_history_returns_loading_to_a_retryable_state() {
    const CHAT_ID: &str = "19:history-unavailable@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert_eq!(
        chat.remote_chats.history_state(0),
        Some(super::super::remote::RemoteChatHistoryState::Loading)
    );

    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::unavailable_for_test(CHAT_ID)
    ));
    assert_eq!(
        chat.remote_chats.history_state(0),
        Some(super::super::remote::RemoteChatHistoryState::NotRequested)
    );
    assert_eq!(chat.selected_messages()[0].text, "Preview");

    // The next directory refresh retries the read.
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Preview"));
    assert!(chat.take_remote_chat_messages_request().is_some());

    // A late failure never downgrades a completed history.
    assert!(chat.apply_remote_chat_messages_projection(remote_history(CHAT_ID, "Loaded")));
    assert!(chat.apply_remote_chat_messages_projection(
        RemoteChatMessagesProjection::unavailable_for_test(CHAT_ID)
    ));
    assert_eq!(
        chat.remote_chats.history_state(0),
        Some(super::super::remote::RemoteChatHistoryState::Ready)
    );
}

#[test]
fn unavailable_pinned_and_members_reads_show_unavailable_and_reopen_retries() {
    const CHAT_ID: &str = "19:panels-unavailable@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    chat.toggle_remote_chat_pinned_messages(0);
    assert!(chat.take_remote_chat_pinned_messages_request().is_some());
    assert!(chat.apply_remote_chat_pinned_messages_projection(
        RemotePinnedChatMessagesProjection::unavailable_for_test(CHAT_ID)
    ));
    assert!(matches!(
        chat.remote_chats.pinned_messages_panel(0),
        Some(super::super::remote::RemotePinnedMessagesPanel::Unavailable)
    ));
    chat.toggle_remote_chat_pinned_messages(0);
    assert!(chat.remote_chats.pinned_messages_panel(0).is_none());
    chat.toggle_remote_chat_pinned_messages(0);
    assert!(chat.take_remote_chat_pinned_messages_request().is_some());

    chat.toggle_remote_chat_members(0);
    assert!(chat.take_remote_chat_members_request().is_some());
    assert!(chat.apply_remote_chat_members_projection(
        RemoteChatMembersProjection::unavailable_for_test(CHAT_ID)
    ));
    assert!(matches!(
        chat.remote_chats.members_panel(0),
        Some(super::super::remote::RemoteChatMembersPanel::Unavailable)
    ));
    chat.toggle_remote_chat_members(0);
    chat.toggle_remote_chat_members(0);
    assert!(chat.take_remote_chat_members_request().is_some());
}

#[test]
fn leaving_or_clearing_remote_chat_cancels_its_history_read() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:cancel-history@thread.v2",
        "Directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    chat.select_workspace_conversation(0);
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.take_remote_chat_messages_request().is_none());

    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    chat.clear_remote_projection();
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.take_remote_chat_messages_request().is_none());
}

#[test]
fn selected_remote_pinned_messages_are_target_bound_session_only_and_read_only() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:pins-first@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "First chat",
            "AL",
            false,
            Vec::new(),
        ),
        RemoteChatConversation::for_test(
            "19:pins-second@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Second chat",
            "GH",
            false,
            Vec::new(),
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    let expected = remote_pinned_messages(
        "19:pins-first@thread.v2",
        [("Ada Lovelace", "2026-11-06T10:31:00Z", "Pinned plan", false)],
    );
    let stale = remote_pinned_messages(
        "19:pins-second@thread.v2",
        [("Grace Hopper", "2026-11-06T10:32:00Z", "Wrong chat", false)],
    );

    chat.toggle_remote_chat_pinned_messages(0);
    assert_eq!(
        chat.take_remote_chat_pinned_messages_request(),
        Some(expected.chat()),
        "only the selected opaque chat target may cross to the controller"
    );
    assert!(!chat.apply_remote_chat_pinned_messages_projection(stale));
    assert!(chat.apply_remote_chat_pinned_messages_projection(expected));
    assert!(matches!(
        chat.remote_chats.pinned_messages_panel(0),
        Some(super::super::remote::RemotePinnedMessagesPanel::Ready(messages))
            if messages.len() == 1
                && messages[0].sender_name == "Ada Lovelace"
                && messages[0].plain_text == "Pinned plan"
    ));
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());

    chat.select_remote_conversation(1);
    assert!(
        chat.take_remote_chat_pinned_messages_cancellation_requested(),
        "leaving the selected chat must cancel only its contextual pins read"
    );
    assert!(chat.take_remote_chat_pinned_messages_request().is_none());
    assert!(chat.remote_chats.pinned_messages_panel(0).is_none());
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_pinned_messages_close_clear_and_access_denial_drop_only_panel_state() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_projection(
        "19:pins-lifecycle@thread.v2",
        "Directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    chat.toggle_remote_chat_pinned_messages(0);
    let _ = chat.take_remote_chat_pinned_messages_request();
    chat.toggle_remote_chat_pinned_messages(0);
    assert!(chat.take_remote_chat_pinned_messages_cancellation_requested());
    assert!(chat.remote_chats.pinned_messages_panel(0).is_none());
    assert!(
        !chat.take_remote_chat_messages_cancellation_requested(),
        "closing pins cannot cancel selected message history"
    );

    chat.toggle_remote_chat_pinned_messages(0);
    let _ = chat.take_remote_chat_pinned_messages_request();
    assert!(chat.apply_remote_chat_pinned_messages_projection(
        RemotePinnedChatMessagesProjection::access_denied_for_test("19:pins-lifecycle@thread.v2")
    ));
    assert!(matches!(
        chat.remote_chats.pinned_messages_panel(0),
        Some(super::super::remote::RemotePinnedMessagesPanel::AccessDenied)
    ));

    let mut toast = ToastState::default();
    render_headlessly(|ui| chat.draw_conversation(ui, &mut toast));
    chat.clear_remote_projection();
    assert!(chat.take_remote_chat_pinned_messages_cancellation_requested());
    assert!(chat.remote_chats.is_empty());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_pinned_messages_close_on_chat_surface_exit_and_projection_loss() {
    const CHAT_ID: &str = "19:pins-surface@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Directory preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    let late_result = remote_pinned_messages(
        CHAT_ID,
        [("Ada Lovelace", "2026-11-06T10:31:00Z", "Late pin", false)],
    );
    chat.toggle_remote_chat_pinned_messages(0);
    assert_eq!(
        chat.take_remote_chat_pinned_messages_request(),
        Some(late_result.chat())
    );
    chat.dismiss_contextual_overlays();
    assert!(chat.take_remote_chat_pinned_messages_cancellation_requested());
    assert!(chat.remote_chats.pinned_messages_panel(0).is_none());
    assert!(chat.take_remote_chat_pinned_messages_request().is_none());
    assert!(
        !chat.apply_remote_chat_pinned_messages_projection(late_result),
        "a panel dismissed with the Chat surface must reject an in-flight result"
    );
    assert!(
        !chat.take_remote_chat_messages_cancellation_requested(),
        "leaving the Chat surface cannot cancel ordinary selected history"
    );

    chat.toggle_remote_chat_pinned_messages(0);
    let _ = chat.take_remote_chat_pinned_messages_request();
    chat.apply_remote_projection(RemoteChatProjection::for_test(Vec::new()));
    assert!(chat.take_remote_chat_pinned_messages_cancellation_requested());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert!(
        !chat.apply_remote_chat_pinned_messages_projection(remote_pinned_messages(
            CHAT_ID,
            [("Ada Lovelace", "2026-11-06T10:31:00Z", "Stale pin", false)],
        )),
        "a projection-loss boundary must reject an already-returning pin result"
    );
}

#[test]
fn selected_remote_members_are_target_bound_session_only_and_read_only() {
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:people-first@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "First chat",
            "AL",
            false,
            Vec::new(),
        ),
        RemoteChatConversation::for_test(
            "19:people-second@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Second chat",
            "GH",
            false,
            Vec::new(),
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    let expected = RemoteChatMembersProjection::for_test(
        "19:people-first@thread.v2",
        vec![
            RemoteChatMember::for_test("Ada Lovelace", Some("Owner")),
            RemoteChatMember::for_test("", None),
        ],
    );
    let stale = RemoteChatMembersProjection::for_test(
        "19:people-second@thread.v2",
        vec![RemoteChatMember::for_test("Grace Hopper", None)],
    );

    chat.toggle_remote_chat_members(0);
    assert_eq!(
        chat.take_remote_chat_members_request(),
        Some(expected.chat()),
        "only the selected opaque chat target may cross to the controller"
    );
    assert!(!chat.apply_remote_chat_members_projection(stale));
    assert!(chat.apply_remote_chat_members_projection(expected));
    assert!(matches!(
        chat.remote_chats.members_panel(0),
        Some(super::super::remote::RemoteChatMembersPanel::Ready(members))
            if members.len() == 2
                && members[0].display_name == "Ada Lovelace"
                && members[0].role_label.as_deref() == Some("Owner")
                && members[1].display_name == "Microsoft 365 member"
    ));
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());

    chat.select_remote_conversation(1);
    assert!(
        chat.take_remote_chat_members_cancellation_requested(),
        "leaving the selected chat must cancel only its People read"
    );
    assert!(chat.take_remote_chat_members_request().is_none());
    assert!(chat.remote_chats.members_panel(0).is_none());
    assert_eq!(chat.stored(), stored_before);
}

#[test]
fn remote_members_close_cancels_and_access_denial_drops_only_panel_state() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(
        "19:people-lifecycle@thread.v2",
        "Directory preview",
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();

    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    chat.toggle_remote_chat_members(0);
    assert!(chat.take_remote_chat_members_cancellation_requested());
    assert!(chat.remote_chats.members_panel(0).is_none());
    assert!(
        !chat.take_remote_chat_messages_cancellation_requested(),
        "closing People cannot cancel selected message history"
    );
    assert!(!chat.take_remote_chat_pinned_messages_cancellation_requested());

    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    assert!(chat.apply_remote_chat_members_projection(
        RemoteChatMembersProjection::access_denied_for_test("19:people-lifecycle@thread.v2")
    ));
    assert!(matches!(
        chat.remote_chats.members_panel(0),
        Some(super::super::remote::RemoteChatMembersPanel::AccessDenied)
    ));

    let mut toast = ToastState::default();
    render_headlessly(|ui| chat.draw_conversation(ui, &mut toast));
    chat.clear_remote_projection();
    assert!(chat.take_remote_chat_members_cancellation_requested());
    assert!(chat.remote_chats.is_empty());
}

#[test]
fn remote_members_close_on_chat_surface_exit_and_projection_loss() {
    const CHAT_ID: &str = "19:people-surface@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_projection(CHAT_ID, "Directory preview"));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    let late_result = || {
        RemoteChatMembersProjection::for_test(
            CHAT_ID,
            vec![RemoteChatMember::for_test("Ada Lovelace", None)],
        )
    };

    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    chat.dismiss_contextual_overlays();
    assert!(chat.take_remote_chat_members_cancellation_requested());
    assert!(chat.remote_chats.members_panel(0).is_none());
    assert!(!chat.apply_remote_chat_members_projection(late_result()));

    chat.toggle_remote_chat_members(0);
    let _ = chat.take_remote_chat_members_request();
    chat.apply_remote_projection(RemoteChatProjection::for_test(Vec::new()));
    assert!(chat.take_remote_chat_members_cancellation_requested());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert!(!chat.apply_remote_chat_members_projection(late_result()));
}
