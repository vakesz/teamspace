use super::super::{ChatAction, ChatQuickView, ChatState, ConversationSelection};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMessage, RemoteChatMessagesProjection,
    RemoteChatProjection, WorkspaceIntentId,
};

fn remote_chat_with_messages(
    chat_id: &str,
    title: &str,
    subtitle: &str,
    messages: Vec<RemoteChatMessage>,
) -> RemoteChatConversation {
    RemoteChatConversation::for_test(
        chat_id,
        RemoteChatKind::OneOnOne,
        title,
        subtitle,
        "XX",
        false,
        messages,
    )
}

fn id_message(id: &str, text: &str, time: &str, mentions_me: bool) -> RemoteChatMessage {
    RemoteChatMessage::for_test_with_id_and_current_user_mention(id, "Ada", time, text, mentions_me)
}

#[test]
fn selected_chat_keeps_rows_but_takes_the_fresh_directory_summary() {
    const CHAT_ID: &str = "19:fresh-summary@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        remote_chat_with_messages(
            CHAT_ID,
            "Old title",
            "Old subtitle",
            vec![id_message("m1", "Old preview", "09:00", false)],
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![
                id_message("m0", "Loaded first", "08:00", false),
                id_message("m1", "Loaded last", "09:00", false),
            ],
        ))
    );
    assert_eq!(
        chat.remote_chats.get(0).expect("row").last_preview(),
        "Loaded last"
    );

    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        remote_chat_with_messages(
            CHAT_ID,
            "New title",
            "New subtitle",
            vec![id_message("m2", "Newest arrived", "10:15", false)],
        ),
    ]));

    // The loaded rows are kept (stale) ...
    let texts = chat
        .selected_messages()
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(texts, ["Loaded first", "Loaded last"]);
    // ... but the sidebar summary follows the fresh projection.
    let row = chat.remote_chats.get(0).expect("row");
    assert_eq!(row.title(), "New title");
    assert_eq!(row.last_preview(), "Newest arrived");
    assert_eq!(row.last_time(), "10:15");
    // The search key follows both, so either text finds the chat.
    assert!(row.matches_query("newest arrived"));
    assert!(row.matches_query("loaded first"));
    assert!(row.matches_query("new title"));
    assert!(!row.matches_query("old title"));
}

#[test]
fn remote_search_key_tracks_history_and_does_not_match_across_fields() {
    const CHAT_ID: &str = "19:search-key@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        remote_chat_with_messages(
            CHAT_ID,
            "Design Sync",
            "Weekly",
            vec![id_message("m1", "Preview Text", "09:00", false)],
        ),
    ]));
    let row = chat.remote_chats.get(0).expect("row");
    assert!(row.matches_query("design sync"));
    assert!(row.matches_query("preview text"));
    // Fields are joined by a newline, so a one-line query cannot span them.
    assert!(!row.matches_query("sync weekly"));
    assert!(!row.matches_query("needle"));

    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![id_message("m1", "Needle In History", "09:00", false)],
        ))
    );
    assert!(
        chat.remote_chats
            .get(0)
            .expect("row")
            .matches_query("needle in")
    );
    assert!(
        chat.search_results("needle in")
            .iter()
            .any(|result| result.label == "Design Sync")
    );
}

#[test]
fn history_mention_survives_directory_refresh_until_a_newer_message_arrives() {
    let (chat_a, chat_b) = ("19:mention-a@thread.v2", "19:mention-b@thread.v2");
    let directory = |preview_id: &str, preview_text: &str| {
        RemoteChatProjection::for_test(vec![
            remote_chat_with_messages(
                chat_a,
                "A",
                "a",
                vec![id_message(preview_id, preview_text, "09:00", false)],
            ),
            remote_chat_with_messages(
                chat_b,
                "B",
                "b",
                vec![id_message("b1", "B preview", "09:00", false)],
            ),
        ])
    };
    let mut chat = ChatState::default();
    chat.apply_remote_projection(directory("m2", "Preview"));
    assert!(
        !chat
            .remote_chats
            .get(0)
            .expect("row")
            .has_current_user_mention()
    );

    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            chat_a,
            vec![
                id_message("m1", "Hey @me", "08:00", true),
                id_message("m2", "Preview", "09:00", false),
            ],
        ))
    );
    assert!(
        chat.remote_chats
            .get(0)
            .expect("row")
            .has_current_user_mention()
    );

    // Select the other chat, so a refresh replaces A's rows with its
    // mention-free preview.
    chat.select_remote_conversation(1);
    let _ = chat.take_remote_chat_messages_request();
    chat.quick_view = ChatQuickView::Mentions;
    chat.apply_remote_projection(directory("m2", "Preview"));
    let row = chat.remote_chats.get(0).expect("row");
    assert_eq!(
        row.messages().len(),
        1,
        "A's rows are the directory preview"
    );
    assert!(
        row.has_current_user_mention(),
        "same newest id keeps the bit"
    );
    assert!(chat.matches_quick_view(ConversationSelection::Remote(0)));

    // A newer preview id that is not from the loaded history clears it.
    chat.apply_remote_projection(directory("m3", "Newer"));
    assert!(
        !chat
            .remote_chats
            .get(0)
            .expect("row")
            .has_current_user_mention()
    );
    assert!(!chat.matches_quick_view(ConversationSelection::Remote(0)));
}

#[test]
fn deleted_history_mentions_and_access_denial_do_not_count() {
    const CHAT_ID: &str = "19:mention-deleted@thread.v2";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        remote_chat_with_messages(
            CHAT_ID,
            "A",
            "a",
            vec![id_message("m1", "Preview", "09:00", false)],
        ),
    ]));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            vec![RemoteChatMessage::for_test_with_current_user_mention(
                "Ada", "09:00", "gone", false, true, true,
            )],
        ))
    );
    assert!(
        !chat
            .remote_chats
            .get(0)
            .expect("row")
            .has_current_user_mention()
    );
}

#[test]
fn hidden_drafts_are_bounded_but_bound_sends_are_never_evicted() {
    let conversations = |count: usize| {
        RemoteChatProjection::for_test(
            (0..count)
                .map(|index| {
                    RemoteChatConversation::for_test(
                        format!("19:hidden-{index}@thread.v2"),
                        RemoteChatKind::OneOnOne,
                        format!("Chat {index}"),
                        "sub",
                        "CC",
                        false,
                        vec![],
                    )
                })
                .collect(),
        )
    };
    let mut chat = ChatState::default();
    let _ = chat.apply_remote_projection(conversations(61));
    for index in 0..61 {
        chat.remote_chats
            .get_mut(index)
            .expect("remote chat exists")
            .draft_mut()
            .push_str("unsent draft");
    }
    // One chat has an accepted send bound to a controller intent.
    chat.queue_remote_message(60)
        .expect("remote message must queue");
    let submission = match chat.take_actions().as_slice() {
        [ChatAction::SendRemoteMessage { submission, .. }] => *submission,
        _ => panic!("exactly one remote action must be queued"),
    };
    let intent = WorkspaceIntentId::for_test(61);
    assert!(chat.bind_remote_submission(submission, intent));

    // Every chat leaves the directory at once.
    let _ = chat.apply_remote_projection(conversations(0));
    assert_eq!(chat.remote_chats.hidden_count(), 50);
    // The bound send survived eviction and can still resolve.
    assert!(chat.remote_send_failed(intent));
}
