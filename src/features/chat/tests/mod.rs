use super::{ChatAction, ChatState};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMessage, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, RemoteChatProjection, RemoteMessageReaction,
    RemotePinnedChatMessagesProjection,
};
use crate::shared::reaction::ReactionKind;

mod local_conversations;
mod local_messages;
mod remote_arrivals;
mod remote_attachments;
mod remote_edit_delete;
mod remote_history;
mod remote_mention_send;
mod remote_mentions;
mod remote_presence;
mod remote_quote_multi;
mod remote_reactions;
mod remote_read_receipts;
mod remote_refresh;
mod remote_search_open;
mod remote_send;

fn send_local_text(chat: &mut ChatState, text: &str) -> u64 {
    chat.selected_composer_mut()
        .expect("test selection has a composer")
        .draft = text.into();
    chat.send();
    let selected = chat.selected;
    let last_index = chat.selected_messages().len().saturating_sub(1);
    chat.timeline_for(selected)
        .and_then(|timeline| timeline.local_id_at(last_index))
        .expect("sent local text must receive a stable identity")
}

fn remote_projection(chat_id: &str, content: &str) -> RemoteChatProjection {
    RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
        chat_id,
        RemoteChatKind::OneOnOne,
        "Ada Lovelace",
        "Design sync",
        "AL",
        true,
        vec![RemoteChatMessage::for_test(
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            content,
            false,
            false,
        )],
    )])
}

fn remote_history(chat_id: &str, content: &str) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(
        chat_id,
        vec![RemoteChatMessage::for_test(
            "Ada Lovelace",
            "2026-11-06T10:31:00Z",
            content,
            false,
            false,
        )],
    )
}

fn remote_pinned_messages<'a>(
    chat_id: &str,
    messages: impl IntoIterator<Item = (&'a str, &'a str, &'a str, bool)>,
) -> RemotePinnedChatMessagesProjection {
    RemotePinnedChatMessagesProjection::for_test(
        chat_id,
        messages
            .into_iter()
            .map(|(sender, sent_at, text, deleted)| {
                RemoteChatMessage::for_test(sender, sent_at, text, false, deleted)
            })
            .collect(),
    )
}

fn remote_quote_source_history(
    chat_id: &str,
    message_id: &str,
    content: &str,
) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(
        chat_id,
        vec![RemoteChatMessage::for_test_with_reactions(
            message_id,
            "Ada Lovelace",
            "2026-11-06T10:31:00Z",
            content,
            false,
            false,
            Vec::new(),
        )],
    )
}

fn remote_reaction_projection(
    chat_id: &str,
    message_id: &str,
    content: &str,
    like_count: usize,
    like_selected: bool,
) -> RemoteChatProjection {
    RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
        chat_id,
        RemoteChatKind::OneOnOne,
        "Ada Lovelace",
        "Reaction design sync",
        "AL",
        true,
        vec![RemoteChatMessage::for_test_with_reactions(
            message_id,
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            content,
            false,
            false,
            vec![RemoteMessageReaction::for_test(
                ReactionKind::Like,
                like_count,
                like_selected,
            )],
        )],
    )])
}

fn remote_reaction_history(
    chat_id: &str,
    message_id: &str,
    content: &str,
    like_count: usize,
    like_selected: bool,
) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(
        chat_id,
        vec![RemoteChatMessage::for_test_with_reactions(
            message_id,
            "Ada Lovelace",
            "2026-11-06T10:31:00Z",
            content,
            false,
            false,
            vec![RemoteMessageReaction::for_test(
                ReactionKind::Like,
                like_count,
                like_selected,
            )],
        )],
    )
}

fn remote_reactions_projection(
    chat_id: &str,
    message_id: &str,
    content: &str,
    reactions: Vec<RemoteMessageReaction>,
) -> RemoteChatProjection {
    RemoteChatProjection::for_test(vec![RemoteChatConversation::for_test(
        chat_id,
        RemoteChatKind::OneOnOne,
        "Ada Lovelace",
        "Reaction design sync",
        "AL",
        true,
        vec![RemoteChatMessage::for_test_with_reactions(
            message_id,
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            content,
            false,
            false,
            reactions,
        )],
    )])
}

fn remote_reactions_history(
    chat_id: &str,
    message_id: &str,
    content: &str,
    reactions: Vec<RemoteMessageReaction>,
) -> RemoteChatMessagesProjection {
    RemoteChatMessagesProjection::for_test(
        chat_id,
        vec![RemoteChatMessage::for_test_with_reactions(
            message_id,
            "Ada Lovelace",
            "2026-11-06T10:31:00Z",
            content,
            false,
            false,
            reactions,
        )],
    )
}

fn queue_remote_text(chat: &mut ChatState, text: &str) -> super::RemoteSubmissionId {
    queue_remote_text_with_importance(chat, text, RemoteChatMessageImportance::Normal)
}

fn queue_remote_text_with_importance(
    chat: &mut ChatState,
    text: &str,
    expected_importance: RemoteChatMessageImportance,
) -> super::RemoteSubmissionId {
    let conversation = chat
        .remote_chats
        .get_mut(0)
        .expect("remote test chat must exist");
    conversation.set_composer_importance(expected_importance);
    conversation.draft_mut().push_str(text);
    chat.queue_remote_message(0)
        .expect("remote test message must queue");
    let actions = chat.take_actions();
    assert_eq!(actions.len(), 1);
    match actions.into_iter().next().expect("one remote action") {
        ChatAction::SendRemoteMessage {
            submission,
            chat: target,
            body,
            importance,
            attachment: _,
        } => {
            assert_eq!(
                body.plain_projection(),
                crate::integrations::ComposedBody::from_markdown_lite(text.trim())
                    .plain_projection()
            );
            assert_eq!(importance, expected_importance);
            assert!(
                !format!("{target:?}").contains("19:remote-chat@thread.v2"),
                "opaque Graph targets must not enter incidental debug output"
            );
            submission
        }
        ChatAction::SendLocalMessage { .. } => panic!("remote send must not become a local action"),
        ChatAction::ReplyToRemoteChatMessageWithQuote { .. } => {
            panic!("ordinary remote send must not become a quote reply")
        }
        ChatAction::SetRemoteChatMessageReaction { .. } => {
            panic!("remote send must not become a reaction action")
        }
        ChatAction::EditRemoteChatMessage { .. } | ChatAction::DeleteRemoteChatMessage { .. } => {
            panic!("remote send must not become an edit or deletion")
        }
        ChatAction::UploadRemoteAttachment { .. }
        | ChatAction::ContinueRemoteUpload { .. }
        | ChatAction::CancelRemoteUpload { .. } => {
            panic!("remote send must not become an upload")
        }
    }
}
