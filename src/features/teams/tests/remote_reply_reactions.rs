use super::super::{RemoteTeamsDirectory, TeamsAction, TeamsState};
use super::{
    remote_directory, remote_messages, remote_messages_for_posts, remote_messages_with_like,
    remote_post_target, remote_replies, remote_replies_for_reactions, remote_replies_with_like,
    remote_replies_with_reactions, remote_two_channel_directory,
};
use crate::integrations::{ProviderResourceRef, RemoteMessageReaction, WorkspaceIntentId};
use crate::shared::reaction::ReactionKind;

#[test]
fn remote_channel_reply_like_action_is_route_complete_and_projection_owned() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reply reaction thread");
    let reply_body = "Reactable reply";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Reply reaction thread",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            2,
            false,
        ))
    );

    // The action must agree with the provider-projected current selection;
    // an unchanged or stale UI value cannot create a duplicate set request.
    assert_eq!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, false,),
        None
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true,),
        Some(TeamsAction::SetRemoteChannelReplyReaction {
            team: team_target.clone(),
            channel: channel_target.clone(),
            post: post_target.clone(),
            reply: reply_target.clone(),
            reaction: ReactionKind::Like,
            selected: true,
        })
    );
    assert!(state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
        true,
    ));
    assert!(!state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
        false,
    ));
    assert!(!state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Heart,
        true,
    ));
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true,),
        None,
        "the same visible reply stays gated until a matching projection arrives"
    );

    let intent = WorkspaceIntentId::for_test(711);
    assert!(state.bind_remote_channel_reply_reaction(
        intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));
    assert!(!state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
        true,
    ));
    assert!(!state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(712),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));
    assert!(state.remote_channel_reply_reaction_owned(intent));

    // A matching selected-root-post response that omits this reply cannot
    // release its gate. Only the exact reply target reconciles the click.
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            "A different reply",
            0,
            false,
        ))
    );
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));

    // A late read can contain the same reply but still describe its state
    // before the accepted mutation. It must not unlock the exact reply gate
    // and allow a duplicate set request.
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            2,
            false,
        ))
    );
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true,),
        None,
        "a stale copy of the exact reply cannot release its set gate"
    );

    // The feature reflects the later provider projection rather than applying
    // a local count/selection update when the command was accepted.
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            3,
            true,
        ))
    );
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, false),
        Some(TeamsAction::SetRemoteChannelReplyReaction {
            team: team_target,
            channel: channel_target,
            post: post_target,
            reply: ProviderResourceRef::local_preview(format!("test-reply:{reply_body}")),
            reaction: ReactionKind::Like,
            selected: false,
        })
    );
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn reply_refresh_keeps_bound_reactions_and_discards_unbound_clicks() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Refresh-bound reply reaction";
    let post_target = remote_post_target(post_title);
    let reply_body = "Reply awaiting refresh";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        post_title,
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 1, false),
                RemoteMessageReaction::new(ReactionKind::Heart, 1, false),
            ],
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    let intent = WorkspaceIntentId::for_test(751);
    assert!(state.bind_remote_channel_reply_reaction(
        intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));
    // The heart click has not entered the controller, so a refresh must not
    // let it become a later, misattributed mutation.
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );

    assert!(state.remote_channel_reply_refresh_action().is_some());
    assert!(state.is_loading_remote_channel_replies());
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Heart,
    ));

    // Refreshing the parent channel also clears the visible reply feed. It
    // must retain the selected root and the controller-bound Like gate so a
    // later matching reply projection remains able to reconcile it.
    state.clear_remote_channel_messages();
    assert!(state.remote.is_loading_messages());
    assert!(state.is_loading_remote_channel_replies());
    assert!(state.remote.selected_reply_targets().is_some());
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        post_title,
    )));

    // The first replacement is stale for Like, so the bound gate remains
    // locked. Heart was unbound and is available again from this fresh view.
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 1, false),
                RemoteMessageReaction::new(ReactionKind::Heart, 1, false),
            ],
        ))
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true,),
        None,
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );

    assert!(
        state.replace_remote_channel_replies(remote_replies_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 2, true),
                RemoteMessageReaction::new(ReactionKind::Heart, 1, false),
            ],
        ))
    );
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, false)
            .is_some()
    );
}

#[test]
fn temporary_directory_omission_preserves_bound_reaction_routes_for_reselection() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Directory omission reaction thread";
    let post_target = remote_post_target(post_title);
    let reply_body = "Directory omission reply";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    let post_intent = WorkspaceIntentId::for_test(761);
    assert!(state.bind_remote_channel_post_reaction(
        post_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    let reply_intent = WorkspaceIntentId::for_test(762);
    assert!(state.bind_remote_channel_reply_reaction(
        reply_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));

    // A directory page can temporarily omit a channel that still exists.
    // Its rows and reply selection must disappear, but controller-bound
    // routes must remain gated instead of becoming duplicate clicks.
    state.replace_remote_directory(RemoteTeamsDirectory::default());
    assert!(!state.remote.has_selection());
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));

    let (restored_directory, restored_team, restored_channel) =
        remote_directory("Engineering", "Planning");
    assert_eq!(restored_team, team_target);
    assert_eq!(restored_channel, channel_target);
    state.replace_remote_directory(restored_directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true,),
        None,
        "reselecting a reappeared channel must not duplicate its bound root write"
    );
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, true,),
        None,
        "reopening the same root must not duplicate its bound reply write"
    );
}

#[test]
fn bound_reaction_tombstones_survive_channel_and_local_navigation() {
    let mut state = TeamsState::default();
    let (directory, team_target, planning, delivery) = remote_two_channel_directory();
    let post_title = "Planning reaction route";
    let post_target = remote_post_target(post_title);
    let reply_body = "Planning reply reaction route";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), planning.clone()));
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            planning.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(771),
        team_target.clone(),
        planning.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            planning.clone(),
            post_target.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(772),
        team_target.clone(),
        planning.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));

    // Both remote-channel and local navigation hide the old view. Neither
    // transition can safely assume an accepted non-idempotent write failed.
    assert!(state.select_remote_channel_for_test(team_target.clone(), delivery));
    state.select_team_channel("Bike Commuters · General");
    assert!(state.select_remote_channel_for_test(team_target.clone(), planning.clone()));
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &planning,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &planning,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));

    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            planning.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true,),
        None,
        "returning to a channel must not duplicate its bound root reaction"
    );
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            planning.clone(),
            post_target.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, true),
        None,
        "returning to a channel must not duplicate its bound reply reaction"
    );
}

#[test]
fn bound_reply_reaction_tombstone_survives_root_switch_and_return() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let first_title = "First root with pending reply reaction";
    let second_title = "Second root";
    let first_post = remote_post_target(first_title);
    let second_post = remote_post_target(second_title);
    let reply_body = "Reply persists through root navigation";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(
        state.replace_remote_channel_messages(remote_messages_for_posts(
            team_target.clone(),
            channel_target.clone(),
            &[first_title, second_title],
        ))
    );
    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            first_post.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(773),
        team_target.clone(),
        channel_target.clone(),
        first_post.clone(),
        reply_target.clone(),
        ReactionKind::Heart,
    ));

    assert!(
        state
            .remote_channel_post_selection_action(second_post)
            .is_some()
    );
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &first_post,
        &reply_target,
        ReactionKind::Heart,
    ));
    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            first_post,
            reply_body,
            1,
            false,
        ))
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(reply_target, ReactionKind::Heart, true),
        None,
        "returning to a root post must not duplicate its bound reply reaction"
    );
}

#[test]
fn transport_loss_preserves_bound_reaction_tombstones_until_an_explicit_reset() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Transport-loss reaction route";
    let post_target = remote_post_target(post_title);
    let reply_body = "Transport-loss reply route";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(774),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            1,
            false,
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(775),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));

    state.clear_remote_directory_preserving_delivery_recovery();
    assert!(!state.remote.has_selection());
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));

    state.clear_remote_directory();
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
}

#[test]
fn remote_reply_reaction_gates_are_bound_to_the_typed_kind() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Cross-kind reply thread";
    let post_target = remote_post_target(post_title);
    let reply_body = "Cross-kind reply";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        post_title,
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 2, false),
                RemoteMessageReaction::new(ReactionKind::Heart, 3, false),
            ],
        ))
    );

    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    let like_intent = WorkspaceIntentId::for_test(731);
    let heart_intent = WorkspaceIntentId::for_test(732);
    assert!(state.bind_remote_channel_reply_reaction(
        like_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Like,
    ));
    assert!(state.bind_remote_channel_reply_reaction(
        heart_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Heart,
    ));

    // A refreshed reply can confirm one kind while another still reflects a
    // pre-write state. Only the confirmed typed target may be unlocked.
    assert!(
        state.replace_remote_channel_replies(remote_replies_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            reply_body,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 3, true),
                RemoteMessageReaction::new(ReactionKind::Heart, 3, false),
            ],
        ))
    );
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Heart,
    ));
    assert_eq!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true,),
        None,
        "a confirmed like cannot release the pending heart mutation"
    );
    assert!(state.remote_channel_reply_reaction_failed(heart_intent));
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target, ReactionKind::Heart, true)
            .is_some()
    );
}

#[test]
fn same_route_reply_replacement_drops_an_unbound_reaction_but_keeps_a_bound_tombstone() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Reply target omission parent";
    let post_target = remote_post_target(post_title);
    let unbound_body = "Unbound reply target disappears";
    let bound_body = "Bound reply target disappears";
    let unbound_reply = ProviderResourceRef::local_preview(format!("test-reply:{unbound_body}"));
    let bound_reply = ProviderResourceRef::local_preview(format!("test-reply:{bound_body}"));
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        post_title,
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_for_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            &[unbound_body, bound_body],
        ))
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(unbound_reply.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(bound_reply.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(782),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        bound_reply.clone(),
        ReactionKind::Like,
    ));
    assert!(state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &unbound_reply,
        ReactionKind::Like,
        true,
    ));

    // The selected root is unchanged, so this replacement authoritatively
    // proves the unbound reply is gone. The already accepted write remains a
    // route-complete tombstone until projection reconciliation or rejection.
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        "A different visible reply",
    )));
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &unbound_reply,
        ReactionKind::Like,
    ));
    assert!(!state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &unbound_reply,
        ReactionKind::Like,
        true,
    ));
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        &bound_reply,
        ReactionKind::Like,
    ));
}

#[test]
fn known_reply_reaction_failure_releases_only_its_exact_reply_intent() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reply failure thread");
    let first_body = "First reactable reply";
    let second_body = "Second reactable reply";
    let first_reply = ProviderResourceRef::local_preview(format!("test-reply:{first_body}"));
    let second_reply = ProviderResourceRef::local_preview(format!("test-reply:{second_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Reply failure thread",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(
        state.replace_remote_channel_replies(remote_replies_for_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            &[first_body, second_body],
        ))
    );

    assert!(
        state
            .remote_channel_reply_reaction_action(first_reply.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(second_reply.clone(), ReactionKind::Like, true)
            .is_some()
    );
    let first_intent = WorkspaceIntentId::for_test(713);
    let second_intent = WorkspaceIntentId::for_test(714);
    assert!(state.bind_remote_channel_reply_reaction(
        first_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        first_reply.clone(),
        ReactionKind::Like,
    ));
    assert!(state.bind_remote_channel_reply_reaction(
        second_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        second_reply.clone(),
        ReactionKind::Like,
    ));

    assert!(!state.remote_channel_reply_reaction_failed(WorkspaceIntentId::for_test(715)));
    assert!(state.remote_channel_reply_reaction_failed(first_intent));
    assert!(
        state
            .remote_channel_reply_reaction_action(first_reply, ReactionKind::Like, true)
            .is_some()
    );
    assert_eq!(
        state.remote_channel_reply_reaction_action(second_reply, ReactionKind::Like, true),
        None,
        "a rejection for one reply must not unlock another pending reply route"
    );
}

#[test]
fn unbound_reply_reaction_gates_clear_on_reply_selection_and_account_transitions() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let first_post = remote_post_target("First reply reaction thread");
    let second_post = remote_post_target("Second reply reaction thread");
    let reply_body = "Reply gate reset";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(
        state.replace_remote_channel_messages(remote_messages_for_posts(
            team_target.clone(),
            channel_target.clone(),
            &[
                "First reply reaction thread",
                "Second reply reaction thread"
            ],
        ))
    );
    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        channel_target.clone(),
        first_post.clone(),
        reply_body,
    )));
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &first_post,
        &reply_target,
        ReactionKind::Like,
    ));

    // Moving to another selected root post clears this unbound UI click. A
    // controller-bound reaction is covered by the tombstone regression below.
    assert!(
        state
            .remote_channel_post_selection_action(second_post)
            .is_some()
    );
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &first_post,
        &reply_target,
        ReactionKind::Like,
    ));

    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        channel_target.clone(),
        first_post.clone(),
        reply_body,
    )));
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    state.clear_remote_directory();
    assert!(!state.remote.reply_reaction_is_pending(
        &team_target,
        &channel_target,
        &first_post,
        &reply_target,
        ReactionKind::Like,
    ));
}
