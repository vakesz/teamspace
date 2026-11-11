use super::super::{
    RemoteChannelDirectory, RemoteChannelMessagesDirectory, RemoteChannelPost,
    RemoteChannelRepliesDirectory, RemoteChannelReply, RemoteTeamDirectory, RemoteTeamsDirectory,
    TeamsAction, TeamsState,
};
use super::{
    remote_directory, remote_messages, remote_messages_for_posts, remote_messages_with_like,
    remote_messages_with_reactions, remote_post_target, remote_replies_with_like,
};
use crate::integrations::{ProviderResourceRef, RemoteMessageReaction, WorkspaceIntentId};
use crate::shared::reaction::ReactionKind;

#[test]
fn remote_root_post_like_action_is_target_bound_and_projection_owned() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reactable root post");
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
            "Reactable root post",
            3,
            false,
        ))
    );

    // The action must agree with the provider-projected current selection;
    // passing the unchanged value cannot turn an old frame into a duplicate.
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, false,),
        None
    );
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true,),
        Some(TeamsAction::SetRemoteChannelPostReaction {
            team: team_target.clone(),
            channel: channel_target.clone(),
            post: post_target.clone(),
            reaction: ReactionKind::Like,
            selected: true,
        })
    );
    assert!(state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
        true,
    ));
    assert!(!state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
        false,
    ));
    assert!(!state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Heart,
        true,
    ));
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true,),
        None,
        "the same visible root post stays gated until a matching feed arrives"
    );

    let intent = WorkspaceIntentId::for_test(701);
    assert!(state.bind_remote_channel_post_reaction(
        intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(!state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
        true,
    ));
    assert!(!state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(702),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(state.remote_channel_post_reaction_owned(intent));

    // A same-channel feed that does not contain this root post cannot clear
    // its gate; only the exact provider target can reconcile the reaction.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            "A different root post",
            0,
            false,
        ))
    );
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));

    // The exact post can arrive from a read that raced the mutation. Its
    // unchanged provider selection must not unlock the gate or permit another
    // set request.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            "Reactable root post",
            3,
            false,
        ))
    );
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true,),
        None,
        "a stale copy of the exact root post cannot release its set gate"
    );

    // The feature reflects the later provider projection rather than applying
    // a local count/selection update when the command was accepted.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            "Reactable root post",
            4,
            true,
        ))
    );
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, false,),
        Some(TeamsAction::SetRemoteChannelPostReaction {
            team: team_target,
            channel: channel_target,
            post: post_target,
            reaction: ReactionKind::Like,
            selected: false,
        })
    );
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_post_reaction_gates_are_bound_to_the_typed_kind() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Cross-kind root post";
    let post_target = remote_post_target(post_title);
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 2, false),
                RemoteMessageReaction::new(ReactionKind::Heart, 3, false),
            ],
        ))
    );

    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    let like_intent = WorkspaceIntentId::for_test(721);
    let heart_intent = WorkspaceIntentId::for_test(722);
    assert!(state.bind_remote_channel_post_reaction(
        like_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));
    assert!(state.bind_remote_channel_post_reaction(
        heart_intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Heart,
    ));

    // Only the like has reached the selected feed. A stale heart state must
    // leave the heart gate locked even though it is the same opaque post.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_reactions(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            vec![
                RemoteMessageReaction::new(ReactionKind::Like, 3, true),
                RemoteMessageReaction::new(ReactionKind::Heart, 3, false),
            ],
        ))
    );
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Heart,
    ));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, false)
            .is_some()
    );
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Heart, true,),
        None,
        "a cross-kind projection must not unlock the pending heart mutation"
    );

    assert!(state.remote_channel_post_reaction_failed(heart_intent));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target, ReactionKind::Heart, true)
            .is_some()
    );
}

#[test]
fn same_route_root_replacement_drops_an_unbound_reaction_but_keeps_a_bound_tombstone() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let unbound_title = "Unbound root target disappears";
    let bound_title = "Bound root target disappears";
    let unbound_post = remote_post_target(unbound_title);
    let bound_post = remote_post_target(bound_title);
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(
        state.replace_remote_channel_messages(remote_messages_for_posts(
            team_target.clone(),
            channel_target.clone(),
            &[unbound_title, bound_title],
        ))
    );
    assert!(
        state
            .remote_channel_post_reaction_action(unbound_post.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_post_reaction_action(bound_post.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(781),
        team_target.clone(),
        channel_target.clone(),
        bound_post.clone(),
        ReactionKind::Like,
    ));
    assert!(state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &unbound_post,
        ReactionKind::Like,
        true,
    ));

    // This is an authoritative replacement for the same selected route, not
    // a stale response from another channel. The vanished unbound target can
    // no longer dispatch its staged handoff, but the accepted write remains
    // a tombstone until its eventual outcome is known.
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "A different visible root post",
    )));
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &unbound_post,
        ReactionKind::Like,
    ));
    assert!(!state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &unbound_post,
        ReactionKind::Like,
        true,
    ));
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &bound_post,
        ReactionKind::Like,
    ));
}

#[test]
fn archiving_then_unarchiving_a_channel_drops_unbound_reactions_but_keeps_tombstones() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Archive changes after reaction click";
    let post_target = remote_post_target(post_title);
    let reply_body = "Archive changes after reply click";
    let reply_target = ProviderResourceRef::local_preview(format!("test-reply:{reply_body}"));
    let directory_for_archive = |archived| {
        RemoteTeamsDirectory::microsoft365(vec![RemoteTeamDirectory::microsoft365(
            team_target.clone(),
            "Engineering".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel_target.clone(),
                "Planning".into(),
                String::new(),
                "Standard".into(),
                archived,
            )],
        )])
    };
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
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(783),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Heart,
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
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(784),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Heart,
    ));
    assert!(state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
        true,
    ));
    assert!(state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
        true,
    ));

    // A directory refresh can archive the channel after the UI click but
    // before its coordinator vector drains. The unbound gates disappear with
    // that dropped handoff; accepted writes remain route-complete tombstones.
    state.replace_remote_directory(directory_for_archive(true));
    assert!(!state.has_unbound_remote_channel_post_reaction(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
        true,
    ));
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Heart,
    ));
    assert!(!state.has_unbound_remote_channel_reply_reaction(
        &team_target,
        &channel_target,
        &post_target,
        &reply_target,
        ReactionKind::Like,
        true,
    ));
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

    // Once the directory unarchives the same channel, a fresh click may be
    // staged again; the old dropped vector cannot leave its gate stuck.
    state.replace_remote_directory(directory_for_archive(false));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, true)
            .is_some()
    );
}

#[test]
fn deleted_targets_drop_unbound_reactions_and_allow_reappearance_without_losing_tombstones() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Deleted reaction target";
    let post_target = remote_post_target(post_title);
    let reply_body = "Deleted reply reaction target";
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
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_post_reaction(
        WorkspaceIntentId::for_test(785),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Heart,
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
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target.clone(), ReactionKind::Heart, true)
            .is_some()
    );
    assert!(state.bind_remote_channel_reply_reaction(
        WorkspaceIntentId::for_test(786),
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        reply_target.clone(),
        ReactionKind::Heart,
    ));

    // The selected route still matches, but its exact target is now deleted.
    // A feature-only click is no longer dispatchable, while an accepted write
    // stays a tombstone for its eventual provider outcome.
    assert!(
        state.replace_remote_channel_messages(RemoteChannelMessagesDirectory::microsoft365(
            team_target.clone(),
            channel_target.clone(),
            vec![RemoteChannelPost::microsoft365(
                post_target.clone(),
                "Ada Lovelace".into(),
                "2026-11-06T10:30:00Z".into(),
                post_title.into(),
                "A deleted session-only Microsoft 365 root post.".into(),
                true,
                false,
                Vec::new(),
            )],
        ))
    );
    assert!(
        state.replace_remote_channel_replies(RemoteChannelRepliesDirectory::microsoft365(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
            vec![RemoteChannelReply::microsoft365(
                reply_target.clone(),
                "Grace Hopper".into(),
                "2026-11-06T10:35:00Z".into(),
                reply_body.into(),
                true,
                false,
                Vec::new(),
            )],
        ))
    );
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Heart,
    ));
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

    // If the provider later projects both targets again, a new Like click is
    // available. The old unbound handoff cannot leave either control stuck.
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
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_reply_reaction_action(reply_target, ReactionKind::Like, true)
            .is_some()
    );
}

#[test]
fn known_reaction_failure_releases_only_its_exact_root_post_intent() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let first_post = remote_post_target("First reactable post");
    let second_post = remote_post_target("Second reactable post");
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
            &["First reactable post", "Second reactable post"],
        ))
    );

    assert!(
        state
            .remote_channel_post_reaction_action(first_post.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(
        state
            .remote_channel_post_reaction_action(second_post.clone(), ReactionKind::Like, true)
            .is_some()
    );
    let first_intent = WorkspaceIntentId::for_test(703);
    let second_intent = WorkspaceIntentId::for_test(704);
    assert!(state.bind_remote_channel_post_reaction(
        first_intent,
        team_target.clone(),
        channel_target.clone(),
        first_post.clone(),
        ReactionKind::Like,
    ));
    assert!(state.bind_remote_channel_post_reaction(
        second_intent,
        team_target.clone(),
        channel_target.clone(),
        second_post.clone(),
        ReactionKind::Like,
    ));

    assert!(!state.remote_channel_post_reaction_failed(WorkspaceIntentId::for_test(705)));
    assert!(state.remote_channel_post_reaction_failed(first_intent));
    assert!(
        state
            .remote_channel_post_reaction_action(first_post, ReactionKind::Like, true)
            .is_some()
    );
    assert_eq!(
        state.remote_channel_post_reaction_action(second_post, ReactionKind::Like, true),
        None,
        "a rejection for one root post must not unlock another pending route"
    );
}

#[test]
fn refreshing_remote_root_posts_discards_only_unbound_reaction_clicks() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Clear reaction gate");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Clear reaction gate",
    )));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true)
            .is_some()
    );

    // No controller intent was bound, so this is only an unowned UI click.
    // Replacing the transient feed must not let it outlive the refresh.
    state.clear_remote_channel_messages();
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target,
        channel_target,
        "Clear reaction gate",
    )));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target, ReactionKind::Like, true)
            .is_some()
    );
}

#[test]
fn bound_root_reaction_gate_survives_a_same_channel_refresh() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_title = "Refresh-bound root reaction";
    let post_target = remote_post_target(post_title);
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
    let intent = WorkspaceIntentId::for_test(741);
    assert!(state.bind_remote_channel_post_reaction(
        intent,
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        ReactionKind::Like,
    ));

    state.clear_remote_channel_messages();
    assert!(state.remote.is_loading_messages());
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));

    // A raced refresh still reports the old provider selection, so the gate
    // must remain locked rather than permitting a duplicate set reaction.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            1,
            false,
        ))
    );
    assert!(state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert_eq!(
        state.remote_channel_post_reaction_action(post_target.clone(), ReactionKind::Like, true),
        None,
    );

    // Only a matching selected feed with the requested typed state releases
    // the controller-bound gate.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team_target.clone(),
            channel_target.clone(),
            post_title,
            2,
            true,
        ))
    );
    assert!(!state.remote.post_reaction_is_pending(
        &team_target,
        &channel_target,
        &post_target,
        ReactionKind::Like,
    ));
    assert!(
        state
            .remote_channel_post_reaction_action(post_target, ReactionKind::Like, false)
            .is_some()
    );
}
