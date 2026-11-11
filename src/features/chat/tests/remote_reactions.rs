use super::super::{ChatAction, ChatState, ConversationSelection, MessageAction};
use super::{
    remote_reaction_history, remote_reaction_projection, remote_reactions_history,
    remote_reactions_projection,
};
use crate::integrations::{
    RemoteChatConversation, RemoteChatKind, RemoteChatMessagesProjection, RemoteChatProjection,
    RemoteMessageReaction, WorkspaceIntentId,
};
use crate::shared::reaction::ReactionKind;
use crate::ui::toast::ToastState;

#[test]
fn remote_chat_like_reaction_is_target_bound_and_waits_for_matching_provider_state() {
    const CHAT_ID: &str = "19:reaction-chat@thread.v2";
    const MESSAGE_ID: &str = "1700000000000";
    let mut chat = ChatState::default();
    let stored_before = chat.stored();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID,
        MESSAGE_ID,
        "A reactable remote message",
        2,
        false,
    ));
    chat.select_remote_conversation(0);
    // The global directory's recent-message preview stays read-only while
    // the selected, target-bound history is loading. It cannot originate a
    // reaction that only the `ChatMessages` projection can reconcile.
    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(chat.take_actions().is_empty());

    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reactable remote message",
            2,
            false,
        ))
    );

    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    let action = chat
        .take_actions()
        .into_iter()
        .next()
        .expect("a current reaction-capable remote message must create one action");
    let (chat_target, message_target) = match action {
        ChatAction::SetRemoteChatMessageReaction {
            chat: target,
            message,
            reaction,
            selected,
        } => {
            assert_eq!(reaction, ReactionKind::Like);
            assert!(selected);
            assert!(
                !format!("{target:?}{message:?}").contains(CHAT_ID),
                "opaque provider targets must not enter incidental debug output"
            );
            (target, message)
        }
        ChatAction::SendLocalMessage { .. }
        | ChatAction::SendRemoteMessage { .. }
        | ChatAction::UploadRemoteAttachment { .. }
        | ChatAction::ContinueRemoteUpload { .. }
        | ChatAction::CancelRemoteUpload { .. }
        | ChatAction::ReplyToRemoteChatMessageWithQuote { .. }
        | ChatAction::EditRemoteChatMessage { .. }
        | ChatAction::DeleteRemoteChatMessage { .. } => {
            panic!("remote reaction must not become a send action")
        }
    };
    let intent = WorkspaceIntentId::for_test(841);
    assert!(chat.bind_remote_chat_reaction(
        intent,
        chat_target.clone(),
        message_target.clone(),
        ReactionKind::Like,
    ));
    assert!(chat.remote_chat_reaction_owned(intent));

    // The app may refresh the global directory while the delivery-specific
    // selected-history read is still pending. Even an exact message with the
    // desired state there is not the targeted `ChatMessages` reconciliation
    // and must not unlock this interaction.
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID,
        MESSAGE_ID,
        "A reactable remote message",
        3,
        true,
    ));
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the directory message must remain present")
            .pending
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(chat.take_actions().is_empty());

    // An exact message may arrive from a read that raced the accepted write.
    // Its unchanged selected state cannot release the gate or allow a second
    // set request.
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reactable remote message",
            2,
            false,
        ))
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the selected history row must remain present")
            .pending
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(chat.take_actions().is_empty());

    // Only a matching provider-owned selection releases the gate. The view
    // then offers the inverse action without changing local persistence.
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reactable remote message",
            3,
            true,
        ))
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the selected history row must remain present")
            .pending
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction,
            selected: false,
            ..
        }] if reaction == &ReactionKind::Like
    ));
    assert_eq!(chat.stored(), stored_before);
    assert!(!chat.is_persistence_dirty());
}

#[test]
fn remote_chat_reaction_gates_are_scoped_to_message_and_reaction_kind() {
    const CHAT_ID: &str = "19:multi-reaction-chat@thread.v2";
    const MESSAGE_ID: &str = "multi-reaction-message";
    const CONTENT: &str = "One message with independently reconciled reactions";
    let absent = vec![
        RemoteMessageReaction::new(ReactionKind::Like, 2, false),
        RemoteMessageReaction::new(ReactionKind::Heart, 4, false),
    ];
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        CONTENT,
        absent.clone(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID, MESSAGE_ID, CONTENT, absent,
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Heart),
        &mut toast,
    );
    let actions = chat.take_actions();
    assert_eq!(actions.len(), 2);
    let mut like_target = None;
    let mut heart_target = None;
    for action in actions {
        let ChatAction::SetRemoteChatMessageReaction {
            chat: chat_target,
            message,
            reaction,
            selected,
        } = action
        else {
            panic!("a remote reaction click must remain a reaction action");
        };
        assert!(selected);
        match reaction {
            ReactionKind::Like => like_target = Some((chat_target, message)),
            ReactionKind::Heart => heart_target = Some((chat_target, message)),
            _ => panic!("only the requested supported reactions may be staged"),
        }
    }
    let (like_chat, like_message) = like_target.expect("like action must be staged");
    let (heart_chat, heart_message) = heart_target.expect("heart action must be staged");
    // Bind heart first even though the like request was staged first. This
    // proves a controller intent cannot accidentally attach to the first
    // unbound pending reaction for the same message.
    assert!(chat.bind_remote_chat_reaction(
        WorkspaceIntentId::for_test(902),
        heart_chat,
        heart_message,
        ReactionKind::Heart,
    ));
    assert!(chat.bind_remote_chat_reaction(
        WorkspaceIntentId::for_test(901),
        like_chat,
        like_message,
        ReactionKind::Like,
    ));
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("like view must remain visible")
            .pending
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Heart)
            .expect("heart view must remain visible")
            .pending
    );

    // A known failure unlocks only the matching kind. The heart action must
    // remain locked while it awaits its own selected-history projection.
    assert!(chat.remote_chat_reaction_failed(WorkspaceIntentId::for_test(901)));
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("like view must remain visible")
            .pending
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Heart)
            .expect("heart view must remain visible")
            .pending
    );

    // Re-stage the like and prove it cannot bind to the unambiguous heart
    // intent, then let a projection update only heart. That projection must
    // release only the heart gate even though both target the same message.
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    let like_action = chat
        .take_actions()
        .into_iter()
        .next()
        .expect("the released like action must be stageable again");
    let ChatAction::SetRemoteChatMessageReaction {
        chat: like_chat,
        message: like_message,
        reaction: ReactionKind::Like,
        ..
    } = like_action
    else {
        panic!("the re-staged action must retain its exact reaction kind");
    };
    assert!(chat.bind_remote_chat_reaction(
        WorkspaceIntentId::for_test(903),
        like_chat,
        like_message,
        ReactionKind::Like,
    ));
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID,
            MESSAGE_ID,
            CONTENT,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 2, false),
                RemoteMessageReaction::new(ReactionKind::Heart, 5, true),
            ],
        ))
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("like view must remain visible")
            .pending
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Heart)
            .expect("heart view must remain visible")
            .pending
    );

    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID,
            MESSAGE_ID,
            CONTENT,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 3, true),
                RemoteMessageReaction::new(ReactionKind::Heart, 5, true),
            ],
        ))
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("like view must remain visible")
            .pending
    );
}

#[test]
fn bounded_directory_omission_preserves_bound_reaction_gates_until_reappearance() {
    const CHAT_ID: &str = "19:hidden-reaction-chat@thread.v2";
    const MESSAGE_ID: &str = "hidden-reaction-message";
    const CONTENT: &str = "A message whose reactions remain route-bound";
    let reactions = vec![
        RemoteMessageReaction::new(ReactionKind::Like, 2, false),
        RemoteMessageReaction::new(ReactionKind::Heart, 3, false),
    ];
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        CONTENT,
        reactions.clone(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID,
            MESSAGE_ID,
            CONTENT,
            reactions.clone(),
        ))
    );

    let mut toast = ToastState::default();
    for reaction in [ReactionKind::Like, ReactionKind::Heart] {
        chat.handle_message_action(0, MessageAction::ToggleReaction(reaction), &mut toast);
    }
    let actions = chat.take_actions();
    assert_eq!(actions.len(), 2);
    let mut like_target = None;
    let mut heart_target = None;
    for action in actions {
        let ChatAction::SetRemoteChatMessageReaction {
            chat: target,
            message,
            reaction,
            selected: true,
        } = action
        else {
            panic!("both clicks must retain their exact reaction routes");
        };
        match reaction {
            ReactionKind::Like => like_target = Some((target, message)),
            ReactionKind::Heart => heart_target = Some((target, message)),
            _ => panic!("only the requested reaction kinds may be staged"),
        }
    }
    let (like_chat, like_message) = like_target.expect("like route must be staged");
    let (heart_chat, heart_message) = heart_target.expect("heart route must be staged");
    let like_intent = WorkspaceIntentId::for_test(914);
    let heart_intent = WorkspaceIntentId::for_test(915);
    assert!(chat.bind_remote_chat_reaction(
        like_intent,
        like_chat,
        like_message,
        ReactionKind::Like,
    ));
    assert!(chat.bind_remote_chat_reaction(
        heart_intent,
        heart_chat,
        heart_message,
        ReactionKind::Heart,
    ));

    // A bounded directory page can omit this chat without an explicit user
    // abandonment or account boundary. Bound writes remain in its hidden
    // tombstone; only selected-history work and unbound clicks are cancelled.
    chat.apply_remote_projection(RemoteChatProjection::for_test(Vec::new()));
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.remote_chat_reaction_owned(heart_intent));
    // A definitive failure must still reach a hidden route and release only
    // its matching kind.
    assert!(chat.remote_chat_reaction_failed(like_intent));

    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        CONTENT,
        reactions.clone(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID, MESSAGE_ID, CONTENT, reactions,
        ))
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("reappeared like view must be visible")
            .pending
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Heart)
            .expect("reappeared heart view must be visible")
            .pending
    );

    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Heart),
        &mut toast,
    );
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction: ReactionKind::Like,
            selected: true,
            ..
        }]
    ));
}

#[test]
fn transport_loss_preserves_only_bound_remote_reaction_tombstones() {
    const CHAT_ID: &str = "19:worker-loss-chat@thread.v2";
    const MESSAGE_ID: &str = "worker-loss-message";
    const CONTENT: &str = "A reaction whose delivery remains uncertain";
    let reactions = vec![
        RemoteMessageReaction::new(ReactionKind::Like, 1, false),
        RemoteMessageReaction::new(ReactionKind::Heart, 2, false),
    ];
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        CONTENT,
        reactions.clone(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID,
            MESSAGE_ID,
            CONTENT,
            reactions.clone(),
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    let action = chat
        .take_actions()
        .into_iter()
        .next()
        .expect("the bound reaction must first enter the controller path");
    let ChatAction::SetRemoteChatMessageReaction {
        chat: target,
        message,
        reaction: ReactionKind::Like,
        selected: true,
    } = action
    else {
        panic!("the staged provider action must retain its exact like route");
    };
    let intent = WorkspaceIntentId::for_test(936);
    assert!(chat.bind_remote_chat_reaction(intent, target, message, ReactionKind::Like));

    // This second click has not reached the controller yet. A worker loss
    // must discard it rather than preserving it beside the known-ambiguous
    // bound route above.
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Heart),
        &mut toast,
    );
    assert!(matches!(
        chat.pending_actions.as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction: ReactionKind::Heart,
            selected: true,
            ..
        }]
    ));

    chat.clear_remote_projection_preserving_delivery_recovery();
    assert!(chat.remote_chats.is_empty());
    assert_eq!(chat.selected, ConversationSelection::Workspace(0));
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.take_actions().is_empty());
    assert!(
        chat.remote_chat_reaction_owned(intent),
        "a worker loss cannot prove whether the bound mutation reached Microsoft 365"
    );

    // A later projection can restore the target-bound tombstone. Like stays
    // locked, while the unbound heart click was discarded and can be staged
    // again from the fresh selected-history projection.
    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        CONTENT,
        reactions.clone(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID, MESSAGE_ID, CONTENT, reactions,
        ))
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Heart),
        &mut toast,
    );
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction: ReactionKind::Heart,
            selected: true,
            ..
        }]
    ));

    // An explicit account/disconnect clear remains terminal for every remote
    // tombstone; it must not route an old provider outcome afterward.
    chat.clear_remote_projection();
    assert!(!chat.remote_chat_reaction_owned(intent));
}

#[test]
fn selected_remote_chat_exposes_the_bounded_reaction_palette() {
    const CHAT_ID: &str = "19:palette-chat@thread.v2";
    const MESSAGE_ID: &str = "palette-message";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reactions_projection(
        CHAT_ID,
        MESSAGE_ID,
        "A message ready for the supported reaction palette",
        Vec::new(),
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reactions_history(
            CHAT_ID,
            MESSAGE_ID,
            "A message ready for the supported reaction palette",
            Vec::new(),
        ))
    );

    let views = chat
        .remote_chats
        .reaction_views(0, 0)
        .expect("a selected remote row must expose the bounded palette");
    assert_eq!(
        views.iter().map(|view| view.reaction).collect::<Vec<_>>(),
        ReactionKind::ALL.to_vec()
    );
    assert!(views.iter().all(|view| view.can_react && !view.pending));

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Sad),
        &mut toast,
    );
    assert!(matches!(
        chat.take_actions().as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction: ReactionKind::Sad,
            selected: true,
            ..
        }]
    ));
}

#[test]
fn remote_chat_reaction_gates_and_unbound_actions_clear_with_selection_or_account() {
    const CHAT_ID: &str = "19:reaction-clear@thread.v2";
    const MESSAGE_ID: &str = "reaction-clear-message";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID,
        MESSAGE_ID,
        "A reaction that must not outlive its selection",
        0,
        false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reaction that must not outlive its selection",
            0,
            false,
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("reaction-capable row must remain visible")
            .pending
    );

    // The app has not drained this click yet. Leaving the selected history
    // must discard the unbound action as well as the feature gate.
    chat.select_workspace_conversation(0);
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.take_actions().is_empty());
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the unselected remote row remains session-only")
            .pending
    );

    // Re-selecting proves the previous gate was not retained, while clearing
    // the remote account drops a newly queued unbound reaction as well.
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reaction that must not outlive its selection",
            0,
            false,
        ))
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(matches!(
        chat.pending_actions.as_slice(),
        [ChatAction::SetRemoteChatMessageReaction { selected: true, .. }]
    ));
    chat.clear_remote_projection();
    assert!(chat.take_actions().is_empty());
}

#[test]
fn selected_history_replacement_prunes_unbound_reaction_actions_that_are_no_longer_dispatchable() {
    const CHAT_ID: &str = "19:history-replacement-reaction-chat@thread.v2";
    const MESSAGE_ID: &str = "history-replacement-reaction-message";
    const CONTENT: &str = "A reaction click must not outlive its selected history row";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID, MESSAGE_ID, CONTENT, 0, false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, CONTENT, 0, false,
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(matches!(
        chat.pending_actions.as_slice(),
        [ChatAction::SetRemoteChatMessageReaction {
            reaction: ReactionKind::Like,
            selected: true,
            ..
        }]
    ));

    // The next selected-history result no longer contains this message. The
    // click has not crossed into the controller, so both its gate and action
    // must disappear instead of POSTing an absent provider target later.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            Vec::new()
        ))
    );
    assert!(chat.take_actions().is_empty());

    // Reappearing history proves the missing-target cleanup did not leave the
    // row permanently pending: a fresh click may be staged again.
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, CONTENT, 0, false,
        ))
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the reappeared row must be visible")
            .pending
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );

    // A same-target history can also prove that the desired selection was
    // already reached. It must invalidate the queued action before the app
    // drains it even though the message route itself still exists.
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, CONTENT, 1, true,
        ))
    );
    assert!(chat.take_actions().is_empty());
}

#[test]
fn selected_history_omission_retains_bound_reaction_tombstones_until_matching_state() {
    const CHAT_ID: &str = "19:history-omission-bound-chat@thread.v2";
    const MESSAGE_ID: &str = "history-omission-bound-message";
    const CONTENT: &str = "A bound reaction remains guarded across a missing history row";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID, MESSAGE_ID, CONTENT, 0, false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, CONTENT, 0, false,
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    let action = chat
        .take_actions()
        .into_iter()
        .next()
        .expect("the ready row must stage one route-complete reaction action");
    let ChatAction::SetRemoteChatMessageReaction {
        chat: target,
        message,
        reaction: ReactionKind::Like,
        selected: true,
    } = action
    else {
        panic!("the staged action must retain its exact typed route");
    };
    let intent = WorkspaceIntentId::for_test(951);
    assert!(chat.bind_remote_chat_reaction(intent, target, message, ReactionKind::Like));

    // This is not a terminal result for a controller-bound non-idempotent
    // mutation. Keep its tombstone even though the current selected history
    // has no matching message row.
    assert!(
        chat.apply_remote_chat_messages_projection(RemoteChatMessagesProjection::for_test(
            CHAT_ID,
            Vec::new()
        ))
    );
    assert!(chat.remote_chat_reaction_owned(intent));

    // Once the exact row reappears with the requested provider state, normal
    // reconciliation releases the tombstone.
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID, MESSAGE_ID, CONTENT, 1, true,
        ))
    );
    assert!(
        !chat
            .remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the reappeared row must be visible")
            .pending
    );
}

#[test]
fn navigating_away_keeps_a_bound_remote_reaction_gated_until_return() {
    const CHAT_ID: &str = "19:navigation-reaction-chat@thread.v2";
    const MESSAGE_ID: &str = "navigation-reaction-message";
    let mut chat = ChatState::default();
    chat.apply_remote_projection(remote_reaction_projection(
        CHAT_ID,
        MESSAGE_ID,
        "A reaction command that remains unresolved during navigation",
        1,
        false,
    ));
    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reaction command that remains unresolved during navigation",
            1,
            false,
        ))
    );

    let mut toast = ToastState::default();
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    let action = chat
        .take_actions()
        .into_iter()
        .next()
        .expect("a ready remote like must stage one route-complete action");
    let ChatAction::SetRemoteChatMessageReaction {
        chat: target,
        message,
        reaction: ReactionKind::Like,
        selected: true,
    } = action
    else {
        panic!("the staged action must retain its typed route");
    };
    let intent = WorkspaceIntentId::for_test(916);
    assert!(chat.bind_remote_chat_reaction(intent, target, message, ReactionKind::Like,));

    chat.select_workspace_conversation(0);
    assert!(chat.take_remote_chat_messages_cancellation_requested());
    assert!(chat.remote_chat_reaction_owned(intent));

    chat.select_remote_conversation(0);
    let _ = chat.take_remote_chat_messages_request();
    assert!(
        chat.apply_remote_chat_messages_projection(remote_reaction_history(
            CHAT_ID,
            MESSAGE_ID,
            "A reaction command that remains unresolved during navigation",
            1,
            false,
        ))
    );
    assert!(
        chat.remote_chats
            .reaction_view(0, 0, ReactionKind::Like)
            .expect("the returned reaction row must be visible")
            .pending
    );
    chat.handle_message_action(
        0,
        MessageAction::ToggleReaction(ReactionKind::Like),
        &mut toast,
    );
    assert!(chat.take_actions().is_empty());
}

#[test]
fn remote_local_read_acknowledgement_survives_projection_reorder() {
    let mut chat = ChatState::default();
    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:first@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            true,
            vec![
                crate::integrations::RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "preview-1",
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Hello",
                    false,
                ),
            ],
        ),
        RemoteChatConversation::for_test(
            "19:second@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Planning",
            "GH",
            true,
            vec![
                crate::integrations::RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "preview-1",
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Hello",
                    false,
                ),
            ],
        ),
    ]));

    chat.select_remote_conversation(0);
    assert!(
        !chat
            .remote_chats
            .get(0)
            .expect("the selected remote chat must exist")
            .is_unread()
    );

    chat.apply_remote_projection(RemoteChatProjection::for_test(vec![
        RemoteChatConversation::for_test(
            "19:second@thread.v2",
            RemoteChatKind::OneOnOne,
            "Grace Hopper",
            "Planning",
            "GH",
            true,
            vec![
                crate::integrations::RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "preview-1",
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Hello",
                    false,
                ),
            ],
        ),
        RemoteChatConversation::for_test(
            "19:first@thread.v2",
            RemoteChatKind::OneOnOne,
            "Ada Lovelace",
            "Design sync",
            "AL",
            true,
            vec![
                crate::integrations::RemoteChatMessage::for_test_with_id_and_current_user_mention(
                    "preview-1",
                    "Ada Lovelace",
                    "2026-11-06T10:30:00Z",
                    "Hello",
                    false,
                ),
            ],
        ),
    ]));

    assert_eq!(chat.selected, ConversationSelection::Remote(1));
    assert!(
        !chat
            .remote_chats
            .get(1)
            .expect("the reordered selected remote chat must exist")
            .is_unread()
    );
    assert!(
        chat.remote_chats
            .get(0)
            .expect("the other remote chat must exist")
            .is_unread()
    );
    assert!(!chat.is_persistence_dirty());
}
