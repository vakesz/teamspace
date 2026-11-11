use super::WorkspaceState;
use crate::features::activity::{ActivityAction, StoredActivity};
use crate::features::teams::{
    RemoteChannelDirectory, RemoteChannelMessagesDirectory,
    RemoteChannelPost as RemoteChannelFeedPost, RemoteChannelPostSubmissionId,
    RemoteChannelRepliesDirectory, RemoteChannelReply as RemoteChannelFeedReply,
    RemoteChannelReplySubmissionId, RemoteTeamDirectory, RemoteTeamsDirectory, StoredTeams,
    TeamsAction,
};
use crate::features::threads::{ThreadId, ThreadStore, ThreadTarget};
use crate::integrations::{
    ProviderResourceRef, RemoteChannelMessagesProjection,
    RemoteChannelPost as RemoteChannelProjectionPost, RemoteChannelRepliesProjection,
    WorkspaceCommand, WorkspaceIntentId,
};
use crate::shared::model::Section;
use crate::shared::reaction::ReactionKind;
use crate::ui::reactions::ReactionSeed;
use crate::ui::toast::ToastState;
use std::collections::BTreeMap;

/// Installs the smallest real selected remote channel/feed needed to
/// exercise a feature-produced reaction action. Coordinator tests must not
/// forge a `TeamsAction`: the feature-owned unbound gate is the authority
/// that makes a remote mutation eligible for app handoff.
fn install_remote_channel_post_for_reaction(
    workspace: &mut WorkspaceState,
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
) {
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    let _ = workspace.take_remote_channel_messages_request();
    assert!(workspace.replace_remote_channel_messages(
        RemoteChannelMessagesDirectory::microsoft365(
            team,
            channel,
            vec![RemoteChannelFeedPost::microsoft365(
                post,
                "Ada Lovelace".into(),
                "2026-11-06T10:30:00Z".into(),
                "Remote root post".into(),
                "A session-only root post.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),
    ));
}

#[test]
fn channel_post_actions_stage_a_provider_neutral_command() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();

    workspace.run_teams_action(
        TeamsAction::ChannelPostSubmitted {
            channel: "Bike Commuters · General".into(),
            body: "A staged local post".into(),
        },
        &mut toast,
    );

    assert_eq!(
        workspace.take_transport_commands(),
        vec![WorkspaceCommand::SendChannelPost {
            team: ProviderResourceRef::local_preview("Bike Commuters"),
            channel: ProviderResourceRef::local_preview("Bike Commuters · General"),
            body: "A staged local post".into(),
        }]
    );
    assert!(workspace.take_transport_commands().is_empty());
}

#[test]
fn remote_channel_posts_stay_separate_until_the_app_binds_an_intent() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let submission = RemoteChannelPostSubmissionId::for_test(7);
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-for-post",
        "remote-channel-for-post",
        Vec::new(),
    );
    let team = projection.team();
    let channel = projection.channel();

    workspace.run_teams_action(
        TeamsAction::SendRemoteChannelPost {
            submission,
            team: team.clone(),
            channel: channel.clone(),
            body: "A text-only remote root post".into(),
        },
        &mut toast,
    );

    // Local/thread/calendar commands use the generic durable handoff;
    // this opaque remote submission must wait for the app to allocate and
    // bind a controller intent instead.
    assert!(workspace.take_transport_commands().is_empty());
    assert_eq!(
        workspace.take_remote_channel_post_requests(),
        vec![(
            submission,
            team,
            channel,
            "A text-only remote root post".into(),
        )]
    );
    assert!(workspace.take_remote_channel_post_requests().is_empty());
}

#[test]
fn remote_channel_reactions_keep_the_complete_graph_route_until_app_enqueue() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-for-reaction",
        "remote-channel-for-reaction",
        vec![RemoteChannelProjectionPost::for_test(
            "remote-post-for-reaction",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote root post",
            "A session-only root post.",
            false,
            false,
        )],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();

    install_remote_channel_post_for_reaction(
        &mut workspace,
        team.clone(),
        channel.clone(),
        post.clone(),
    );
    let action = workspace
        .teams_mut()
        .remote_channel_post_reaction_action_for_test(post.clone(), ReactionKind::Like, true)
        .expect("a visible unreacted root post must create a typed action");
    workspace.run_teams_action(action, &mut toast);

    // A provider-backed mutation must not fall through the generic local
    // command path before the app can bind a controller intent.
    assert!(workspace.take_transport_commands().is_empty());
    assert_eq!(
        workspace.take_remote_channel_post_reaction_requests(),
        vec![(team, channel, post, ReactionKind::Like, true)]
    );
    assert!(
        workspace
            .take_remote_channel_post_reaction_requests()
            .is_empty()
    );
}

#[test]
fn same_channel_refresh_drops_an_unbound_root_reaction_before_handoff() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-omitted-root-reaction",
        "remote-channel-omitted-root-reaction",
        vec![RemoteChannelProjectionPost::for_test(
            "remote-post-omitted-root-reaction",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote root post",
            "This target disappears before its action reaches transport.",
            false,
            false,
        )],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    install_remote_channel_post_for_reaction(
        &mut workspace,
        team.clone(),
        channel.clone(),
        post.clone(),
    );
    let action = workspace
        .teams_mut()
        .remote_channel_post_reaction_action_for_test(post, ReactionKind::Like, true)
        .expect("a visible root post must create the feature-owned action");
    workspace.run_teams_action(action, &mut toast);

    // The click is now represented in both feature and coordinator state,
    // but a fresh matching feed proves this unbound target no longer
    // exists. It must never become a Graph write.
    assert!(workspace.replace_remote_channel_messages(
        RemoteChannelMessagesDirectory::microsoft365(team, channel, Vec::new()),
    ));
    assert!(
        workspace
            .take_remote_channel_post_reaction_requests()
            .is_empty()
    );
}

#[test]
fn remote_channel_reply_reactions_keep_the_complete_graph_route_until_app_enqueue() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    // Every target comes from a normalized projection fixture. The second
    // opaque target represents the selected root's reply; the app must
    // preserve it separately from the root-post target.
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-for-reply-reaction",
        "remote-channel-for-reply-reaction",
        vec![
            RemoteChannelProjectionPost::for_test(
                "remote-root-for-reply-reaction",
                "Ada Lovelace",
                "2026-11-06T10:30:00Z",
                "Remote root post",
                "A session-only root post.",
                false,
                false,
            ),
            RemoteChannelProjectionPost::for_test(
                "remote-reply-for-reply-reaction",
                "Grace Hopper",
                "2026-11-06T10:31:00Z",
                "Remote reply target",
                "A session-only selected-root reply.",
                false,
                false,
            ),
        ],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    let reply = projection.posts()[1].target();

    install_remote_channel_post_for_reaction(
        &mut workspace,
        team.clone(),
        channel.clone(),
        post.clone(),
    );
    assert!(workspace.select_remote_channel_post_for_test(
        team.clone(),
        channel.clone(),
        post.clone(),
    ));
    let _ = workspace.take_remote_channel_replies_cancellation_requested();
    assert!(
        workspace.replace_remote_channel_replies(RemoteChannelRepliesDirectory::microsoft365(
            team.clone(),
            channel.clone(),
            post.clone(),
            vec![RemoteChannelFeedReply::microsoft365(
                reply.clone(),
                "Grace Hopper".into(),
                "2026-11-06T10:31:00Z".into(),
                "A session-only selected-root reply.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),)
    );
    let action = workspace
        .teams_mut()
        .remote_channel_reply_reaction_action_for_test(reply.clone(), ReactionKind::Like, true)
        .expect("a visible unreacted reply must create a typed action");
    workspace.run_teams_action(action, &mut toast);

    // The provider-backed reply mutation must not become a generic/local
    // transport command or collapse the selected reply target into its
    // root post before the app owns an intent ID.
    assert!(workspace.take_transport_commands().is_empty());
    assert_eq!(
        workspace.take_remote_channel_reply_reaction_requests(),
        vec![(team, channel, post, reply, ReactionKind::Like, true)]
    );
    assert!(
        workspace
            .take_remote_channel_reply_reaction_requests()
            .is_empty()
    );
}

#[test]
fn same_reply_refresh_drops_an_unbound_reply_reaction_before_handoff() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-omitted-reply-reaction",
        "remote-channel-omitted-reply-reaction",
        vec![
            RemoteChannelProjectionPost::for_test(
                "remote-root-omitted-reply-reaction",
                "Ada Lovelace",
                "2026-11-06T10:30:00Z",
                "Remote root post",
                "A selected root.",
                false,
                false,
            ),
            RemoteChannelProjectionPost::for_test(
                "remote-reply-omitted-reply-reaction",
                "Grace Hopper",
                "2026-11-06T10:31:00Z",
                "Reply target",
                "A reply that disappears before handoff.",
                false,
                false,
            ),
        ],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    let reply = projection.posts()[1].target();
    install_remote_channel_post_for_reaction(
        &mut workspace,
        team.clone(),
        channel.clone(),
        post.clone(),
    );
    assert!(workspace.select_remote_channel_post_for_test(
        team.clone(),
        channel.clone(),
        post.clone(),
    ));
    let _ = workspace.take_remote_channel_replies_cancellation_requested();
    assert!(
        workspace.replace_remote_channel_replies(RemoteChannelRepliesDirectory::microsoft365(
            team.clone(),
            channel.clone(),
            post.clone(),
            vec![RemoteChannelFeedReply::microsoft365(
                reply.clone(),
                "Grace Hopper".into(),
                "2026-11-06T10:31:00Z".into(),
                "A reply that disappears before handoff.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),)
    );
    let action = workspace
        .teams_mut()
        .remote_channel_reply_reaction_action_for_test(reply, ReactionKind::Like, true)
        .expect("a visible reply must create the feature-owned action");
    workspace.run_teams_action(action, &mut toast);

    assert!(
        workspace.replace_remote_channel_replies(RemoteChannelRepliesDirectory::microsoft365(
            team,
            channel,
            post,
            Vec::new()
        ),)
    );
    assert!(
        workspace
            .take_remote_channel_reply_reaction_requests()
            .is_empty()
    );
}

#[test]
fn remote_channel_reply_writes_stay_separate_from_selected_reply_reads() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let submission = RemoteChannelReplySubmissionId::for_test(8);
    let projection = RemoteChannelRepliesProjection::for_test(
        "remote-team-for-reply",
        "remote-channel-for-reply",
        "remote-root-post-for-reply",
        Vec::new(),
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.post();

    // A selected-root read may already be queued when the user submits a
    // reply. The mutation must not replace or consume that independent
    // bounded projection request.
    workspace.run_teams_action(
        TeamsAction::RemoteChannelPostSelected {
            team: team.clone(),
            channel: channel.clone(),
            post: post.clone(),
        },
        &mut toast,
    );

    workspace.run_teams_action(
        TeamsAction::SendRemoteChannelReply {
            submission,
            team: team.clone(),
            channel: channel.clone(),
            post: post.clone(),
            body: "A text-only remote root reply".into(),
        },
        &mut toast,
    );

    // The reply write waits for the app to allocate a controller intent.
    // It must not become a durable/local command or accidentally consume
    // the independent selected-root `ChannelReplies` read slot.
    assert!(workspace.take_transport_commands().is_empty());
    assert_eq!(
        workspace.take_remote_channel_replies_request(),
        Some((team.clone(), channel.clone(), post.clone()))
    );
    assert_eq!(
        workspace.take_remote_channel_reply_requests(),
        vec![(
            submission,
            team,
            channel,
            post,
            "A text-only remote root reply".into(),
        )]
    );
    assert!(workspace.take_remote_channel_reply_requests().is_empty());
}

#[test]
fn remote_account_boundary_discards_unbound_remote_channel_reply_writes() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    let projection = RemoteChannelRepliesProjection::for_test(
        "remote-team-cleared-reply",
        "remote-channel-cleared-reply",
        "remote-root-post-cleared-reply",
        Vec::new(),
    );
    workspace.run_teams_action(
        TeamsAction::SendRemoteChannelReply {
            submission: RemoteChannelReplySubmissionId::for_test(9),
            team: projection.team(),
            channel: projection.channel(),
            post: projection.post(),
            body: "Do not send after the account boundary".into(),
        },
        &mut toast,
    );

    workspace.clear_remote_teams_directory();

    assert!(workspace.take_remote_channel_reply_requests().is_empty());
}

#[test]
fn real_remote_channel_reply_submission_binds_exact_targets_and_lifecycle_outcomes() {
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-bound-reply",
        "remote-channel-bound-reply",
        vec![RemoteChannelProjectionPost::for_test(
            "remote-root-post-bound-reply",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote root post",
            "A session-only root post.",
            false,
            false,
        )],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    assert!(workspace.replace_remote_channel_messages(
        RemoteChannelMessagesDirectory::microsoft365(
            team.clone(),
            channel.clone(),
            vec![RemoteChannelFeedPost::microsoft365(
                post.clone(),
                "Ada Lovelace".into(),
                "2026-11-06T10:30:00Z".into(),
                "Remote root post".into(),
                "A session-only root post.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),
    ));
    assert!(workspace.select_remote_channel_post_for_test(
        team.clone(),
        channel.clone(),
        post.clone(),
    ));

    assert!(
        workspace
            .teams_mut()
            .set_remote_channel_reply_draft_for_test("A bound remote reply.")
    );
    let action = workspace
        .teams_mut()
        .remote_channel_reply_submission_for_test()
        .expect("a selected root with a staged draft must create a reply action");
    let (submission, action_team, action_channel, action_post, body) = match &action {
        TeamsAction::SendRemoteChannelReply {
            submission,
            team,
            channel,
            post,
            body,
        } => (
            *submission,
            team.clone(),
            channel.clone(),
            post.clone(),
            body.clone(),
        ),
        _ => panic!("the real reply submission helper must emit its reply action"),
    };
    assert_eq!(
        (action_team, action_channel, action_post, body),
        (
            team.clone(),
            channel.clone(),
            post.clone(),
            "A bound remote reply.".into(),
        )
    );

    workspace.run_teams_action(action, &mut toast);
    assert_eq!(
        workspace.take_remote_channel_reply_requests(),
        vec![(
            submission,
            team.clone(),
            channel.clone(),
            post.clone(),
            "A bound remote reply.".into(),
        )]
    );

    let delivered = WorkspaceIntentId::for_test(901);
    assert!(workspace.bind_remote_channel_reply_submission(submission, delivered));
    assert!(!workspace.bind_remote_channel_reply_submission(submission, delivered));
    assert!(workspace.remote_channel_reply_retry_scheduled(delivered));
    assert!(workspace.remote_channel_reply_delivered(delivered));

    assert!(
        workspace
            .teams_mut()
            .set_remote_channel_reply_draft_for_test("A definitely rejected reply.")
    );
    let failed_action = workspace
        .teams_mut()
        .remote_channel_reply_submission_for_test()
        .expect("a delivered reply must release the selected composer");
    let failed_submission = match &failed_action {
        TeamsAction::SendRemoteChannelReply { submission, .. } => *submission,
        _ => panic!("the real reply submission helper must emit its reply action"),
    };
    workspace.run_teams_action(failed_action, &mut toast);
    let failed = WorkspaceIntentId::for_test(902);
    assert!(workspace.bind_remote_channel_reply_submission(failed_submission, failed));
    assert!(workspace.remote_channel_reply_failed(failed));

    assert!(
        workspace
            .teams_mut()
            .set_remote_channel_reply_draft_for_test("An uncertain reply.")
    );
    let uncertain_action = workspace
        .teams_mut()
        .remote_channel_reply_submission_for_test()
        .expect("a rejected reply must restore the selected composer");
    let uncertain_submission = match &uncertain_action {
        TeamsAction::SendRemoteChannelReply { submission, .. } => *submission,
        _ => panic!("the real reply submission helper must emit its reply action"),
    };
    workspace.run_teams_action(uncertain_action, &mut toast);
    let uncertain = WorkspaceIntentId::for_test(903);
    assert!(workspace.bind_remote_channel_reply_submission(uncertain_submission, uncertain));
    assert!(workspace.remote_channel_reply_delivery_uncertain(uncertain));
}

#[test]
fn direct_local_channel_selection_drops_a_queued_remote_reply_read() {
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team",
        "remote-channel",
        vec![RemoteChannelProjectionPost::for_test(
            "remote-post",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote root post",
            "A session-only root post.",
            false,
            false,
        )],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    let mut workspace = WorkspaceState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    assert!(workspace.take_remote_channel_replies_cancellation_requested());
    let _ = workspace.take_remote_channel_messages_request();
    assert!(workspace.replace_remote_channel_messages(
        RemoteChannelMessagesDirectory::microsoft365(
            team.clone(),
            channel.clone(),
            vec![RemoteChannelFeedPost::microsoft365(
                post.clone(),
                "Ada Lovelace".into(),
                "2026-11-06T10:30:00Z".into(),
                "Remote root post".into(),
                "A session-only root post.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),
    ));
    assert!(workspace.select_remote_channel_post_for_test(team, channel, post));

    // This mimics the local channel control, which owns its feature state
    // directly and therefore has no coordinator action to accompany it.
    workspace
        .teams_mut()
        .select_team_channel("Bike Commuters · General");

    assert!(workspace.take_remote_channel_replies_cancellation_requested());
    assert!(workspace.take_remote_channel_replies_request().is_none());
}

#[test]
fn direct_local_channel_selection_drops_a_queued_remote_channel_files_read() {
    let projection =
        RemoteChannelMessagesProjection::for_test("remote-team", "remote-channel", Vec::new());
    let team = projection.team();
    let channel = projection.channel();
    let mut workspace = WorkspaceState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    let _ = workspace.take_remote_channel_messages_request();
    assert!(!workspace.take_remote_channel_files_cancellation_requested());

    assert!(workspace.select_remote_channel_files_for_test(team, channel));
    assert!(workspace.take_remote_channel_files_request().is_some());

    // This mimics the local channel control, which owns its feature state
    // directly and therefore has no coordinator action to accompany it.
    workspace
        .teams_mut()
        .select_team_channel("Bike Commuters · General");

    assert!(workspace.take_remote_channel_files_cancellation_requested());
    assert!(workspace.take_remote_channel_files_request().is_none());
}

#[test]
fn selecting_another_remote_channel_drops_a_queued_channel_files_read() {
    let first_projection =
        RemoteChannelMessagesProjection::for_test("remote-team", "remote-channel-a", Vec::new());
    let team = first_projection.team();
    let first_channel = first_projection.channel();
    let second_channel =
        RemoteChannelMessagesProjection::for_test("remote-team", "remote-channel-b", Vec::new())
            .channel();
    let mut workspace = WorkspaceState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![
                RemoteChannelDirectory::microsoft365(
                    first_channel.clone(),
                    "First remote channel".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
                RemoteChannelDirectory::microsoft365(
                    second_channel.clone(),
                    "Second remote channel".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
            ],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), first_channel.clone()));
    let _ = workspace.take_remote_channel_messages_request();
    assert!(workspace.select_remote_channel_files_for_test(team.clone(), first_channel));
    assert!(workspace.take_remote_channel_files_request().is_some());
    assert!(!workspace.take_remote_channel_files_cancellation_requested());

    assert!(workspace.select_remote_channel_for_test(team, second_channel));

    assert!(workspace.take_remote_channel_files_cancellation_requested());
    assert!(workspace.take_remote_channel_files_request().is_none());
}

#[test]
fn refreshing_remote_posts_does_not_cancel_the_same_channel_files_read() {
    let projection =
        RemoteChannelMessagesProjection::for_test("remote-team", "remote-channel", Vec::new());
    let team = projection.team();
    let channel = projection.channel();
    let mut workspace = WorkspaceState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    let _ = workspace.take_remote_channel_messages_request();
    assert!(workspace.select_remote_channel_files_for_test(team.clone(), channel.clone()));
    assert!(workspace.take_remote_channel_files_request().is_some());
    assert!(!workspace.take_remote_channel_files_cancellation_requested());

    let mut toast = ToastState::default();
    workspace.run_teams_action(
        TeamsAction::RemoteChannelSelected { team, channel },
        &mut toast,
    );

    assert!(!workspace.take_remote_channel_files_cancellation_requested());
    assert!(workspace.take_remote_channel_files_request().is_none());
}

#[test]
fn refreshing_remote_posts_keeps_the_same_selected_reply_read() {
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-refresh-replies",
        "remote-channel-refresh-replies",
        vec![RemoteChannelProjectionPost::for_test(
            "remote-post-refresh-replies",
            "Ada Lovelace",
            "2026-11-06T10:30:00Z",
            "Remote root post",
            "A selected root whose reply read must survive a post refresh.",
            false,
            false,
        )],
    );
    let team = projection.team();
    let channel = projection.channel();
    let post = projection.posts()[0].target();
    let mut workspace = WorkspaceState::default();
    workspace.replace_remote_teams_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team.clone(),
            "Remote team".into(),
            String::new(),
            vec![RemoteChannelDirectory::microsoft365(
                channel.clone(),
                "Remote channel".into(),
                String::new(),
                "Standard".into(),
                false,
            )],
        ),
    ]));
    assert!(workspace.select_remote_channel_for_test(team.clone(), channel.clone()));
    let _ = workspace.take_remote_channel_messages_request();
    let _ = workspace.take_remote_channel_replies_cancellation_requested();
    assert!(workspace.replace_remote_channel_messages(
        RemoteChannelMessagesDirectory::microsoft365(
            team.clone(),
            channel.clone(),
            vec![RemoteChannelFeedPost::microsoft365(
                post.clone(),
                "Ada Lovelace".into(),
                "2026-11-06T10:30:00Z".into(),
                "Remote root post".into(),
                "A selected root whose reply read must survive a post refresh.".into(),
                false,
                false,
                Vec::new(),
            )],
        ),
    ));
    assert!(workspace.select_remote_channel_post_for_test(
        team.clone(),
        channel.clone(),
        post.clone(),
    ));
    let _ = workspace.take_remote_channel_replies_cancellation_requested();

    let mut toast = ToastState::default();
    workspace.run_teams_action(
        TeamsAction::RemoteChannelSelected {
            team: team.clone(),
            channel: channel.clone(),
        },
        &mut toast,
    );

    assert!(
        !workspace.take_remote_channel_replies_cancellation_requested(),
        "a same-channel post refresh must not cancel the selected root's reply read"
    );
    assert_eq!(
        workspace.take_remote_channel_replies_request(),
        Some((team, channel, post)),
    );
}

#[test]
fn canonical_team_reply_changes_keep_channel_and_post_identifiers_at_the_transport_boundary() {
    let mut threads = ThreadStore::default();
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture thread must be valid");
    threads
        .append_reply(thread, "I will add the evaluation notes.")
        .expect("fixture reply must be accepted");
    let change = threads
        .take_changes()
        .pop()
        .expect("accepted reply must emit a change");

    assert_eq!(
        super::workspace_command_for_thread_change(change),
        Some(WorkspaceCommand::ReplyToChannelPost {
            team: ProviderResourceRef::local_preview("Juniper Row All Hands"),
            channel: ProviderResourceRef::local_preview("Juniper Row All Hands · Facilities"),
            post: ProviderResourceRef::local_preview("parking-changes"),
            body: "I will add the evaluation notes.".into(),
        })
    );
}

#[test]
fn canonical_team_reactions_keep_the_stable_target_key_at_the_transport_boundary() {
    let mut threads = ThreadStore::default();
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture thread must be valid");
    let target = ThreadTarget::source_reply("gate-question")
        .expect("fixture source reply key must be valid");
    threads
        .toggle_reaction(
            thread,
            target,
            &[ReactionSeed {
                icon: "👍",
                count: 1,
            }],
            "👍",
        )
        .expect("fixture reaction must be accepted");
    let change = threads
        .take_changes()
        .pop()
        .expect("accepted reaction must emit a change");

    assert_eq!(
        super::workspace_command_for_thread_change(change),
        Some(WorkspaceCommand::SetMessageReaction {
            message: ProviderResourceRef::local_preview("parking-changes:reply:gate-question",),
            reaction: "👍".into(),
            selected: true,
        })
    );
}

#[test]
fn activity_only_thread_changes_remain_local() {
    let mut threads = ThreadStore::default();
    let thread = ThreadId::new("Bike Commuters · General", "activity:van-booking")
        .expect("fixture thread must be valid");
    threads
        .append_reply(thread, "I will review the update.")
        .expect("fixture reply must be accepted");
    let change = threads
        .take_changes()
        .pop()
        .expect("accepted reply must emit a change");

    assert_eq!(super::workspace_command_for_thread_change(change), None);
}

#[test]
fn local_reply_reaction_changes_remain_local() {
    let mut threads = ThreadStore::default();
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture thread must be valid");
    let reply = threads
        .append_reply(thread.clone(), "A local reply with a local reaction.")
        .expect("fixture reply must be accepted");
    let _ = threads.take_changes();
    threads
        .toggle_reaction(
            thread,
            ThreadTarget::local_reply(reply.id()).expect("fixture reply ID must be nonzero"),
            &[],
            "❤",
        )
        .expect("local reply reaction must be accepted");
    let change = threads
        .take_changes()
        .pop()
        .expect("accepted reaction must emit a change");

    assert_eq!(super::workspace_command_for_thread_change(change), None);
}

#[test]
fn local_reply_edits_and_deletes_remain_local() {
    let mut threads = ThreadStore::default();
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture thread must be valid");
    let reply = threads
        .append_reply(thread.clone(), "A local reply")
        .expect("fixture reply must be accepted");
    let _ = threads.take_changes();

    threads
        .edit_reply(thread.clone(), reply.id(), "An edited local reply")
        .expect("fixture reply edit must be accepted");
    let edit = threads
        .take_changes()
        .pop()
        .expect("accepted reply edit must emit a change");
    assert_eq!(super::workspace_command_for_thread_change(edit), None);

    threads
        .delete_reply(thread, reply.id())
        .expect("fixture reply delete must be accepted");
    let delete = threads
        .take_changes()
        .pop()
        .expect("accepted reply delete must emit a change");
    assert_eq!(super::workspace_command_for_thread_change(delete), None);
}

#[test]
fn legacy_discussions_import_once_without_replaying_transport_commands() {
    let activity = StoredActivity {
        replies_by_item: BTreeMap::from([
            (
                "parking-changes".into(),
                vec!["Activity-owned legacy reply".into()],
            ),
            (
                "van-booking".into(),
                vec!["Activity-only legacy reply".into()],
            ),
        ]),
        selected_reactions_by_target: BTreeMap::from([(
            "parking-changes:reply:gate-question".into(),
            vec!["❤".into()],
        )]),
        ..Default::default()
    };

    let mut teams = StoredTeams::default();
    let channel = teams
        .channels
        .entry("Juniper Row All Hands · Facilities".into())
        .or_default();
    channel.replies_by_post.insert(
        "parking-changes".into(),
        vec!["Teams-owned legacy reply".into()],
    );
    channel.selected_reactions_by_target.insert(
        "parking-changes:reply:gate-question".into(),
        vec!["👍".into()],
    );

    let mut workspace = WorkspaceState::default();
    workspace.restore_activity(activity);
    workspace.restore_teams(teams);
    workspace
        .import_unmigrated_legacy_threads()
        .expect("valid legacy discussion data must import");

    let parking = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("fixture discussion id must be valid");
    let replies = workspace
        .threads_mut()
        .replies(&parking)
        .iter()
        .map(|reply| reply.body())
        .collect::<Vec<_>>();
    assert_eq!(
        replies,
        ["Activity-owned legacy reply", "Teams-owned legacy reply"]
    );
    assert_eq!(
        workspace
            .threads_mut()
            .reactions(
                &parking,
                &ThreadTarget::source_reply("gate-question")
                    .expect("fixture source reply target must be valid"),
                &[],
            )
            .selected_icons(),
        vec!["❤", "👍"]
    );
    assert!(workspace.legacy_thread_imports_are_confirmed());
    assert!(workspace.take_transport_commands().is_empty());

    workspace
        .import_unmigrated_legacy_threads()
        .expect("confirmed imports must be idempotent");
    assert_eq!(
        workspace.threads_mut().replies(&parking).len(),
        2,
        "a second lifecycle pass must not duplicate imported replies"
    );

    workspace.mark_threads_persisted();
    workspace.clear_migrated_activity_legacy_threads();
    workspace.clear_migrated_teams_legacy_threads();
    assert!(workspace.is_activity_persistence_dirty());
    assert!(workspace.is_teams_persistence_dirty());
}

#[test]
fn channel_meeting_actions_stage_a_provider_neutral_calendar_command() {
    let mut workspace = WorkspaceState::default();

    assert!(workspace.start_channel_meeting());

    assert_eq!(
        workspace.take_transport_commands(),
        vec![WorkspaceCommand::CreateCalendarEvent {
            calendar: ProviderResourceRef::local_preview("local-primary"),
            title: "Bike Commuters · General meeting".into(),
            starts_at: "2026-11-03T14:00:00".into(),
            ends_at: "2026-11-03T14:30:00".into(),
            online_meeting: true,
        }]
    );
    assert!(workspace.take_transport_commands().is_empty());
}

#[test]
fn activity_post_actions_open_the_matching_teams_thread() {
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();

    workspace.run_activity_action(
        ActivityAction::OpenTeamPost {
            channel: "Juniper Row All Hands · Facilities".into(),
            post_key: "parking-changes".into(),
        },
        &mut toast,
    );

    assert_eq!(
        workspace.teams.selected_channel_name(),
        "Juniper Row All Hands · Facilities"
    );
    assert_eq!(workspace.teams.active_thread_key(), Some("parking-changes"));
    assert_eq!(workspace.take_navigation_request(), Some(Section::Teams));
}

fn one_channel_directory(
    team: &ProviderResourceRef,
    channel: &ProviderResourceRef,
) -> RemoteTeamsDirectory {
    RemoteTeamsDirectory::microsoft365(vec![RemoteTeamDirectory::microsoft365(
        team.clone(),
        "Remote team".into(),
        String::new(),
        vec![RemoteChannelDirectory::microsoft365(
            channel.clone(),
            "Remote channel".into(),
            String::new(),
            "Standard".into(),
            false,
        )],
    )])
}

#[test]
fn a_search_hit_opens_its_remote_channel_by_target_and_loads_its_posts() {
    let projection = RemoteChannelMessagesProjection::for_test(
        "remote-team-search-hit",
        "remote-channel-search-hit",
        Vec::new(),
    );
    let (team, channel) = (projection.team(), projection.channel());
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    workspace.replace_remote_teams_directory(one_channel_directory(&team, &channel));

    assert!(workspace.open_remote_channel(team.clone(), channel.clone(), &mut toast));
    assert_eq!(
        workspace.take_remote_channel_messages_request(),
        Some((team.clone(), channel.clone()))
    );
    // Opening the channel already open is allowed and does not reload it.
    assert!(workspace.open_remote_channel(team, channel, &mut toast));
    assert!(workspace.take_remote_channel_messages_request().is_none());
}

#[test]
fn a_search_hit_in_a_channel_outside_the_directory_opens_nothing() {
    let known =
        RemoteChannelMessagesProjection::for_test("team-known", "channel-known", Vec::new());
    let other =
        RemoteChannelMessagesProjection::for_test("team-other", "channel-other", Vec::new());
    let mut workspace = WorkspaceState::default();
    let mut toast = ToastState::default();
    workspace
        .replace_remote_teams_directory(one_channel_directory(&known.team(), &known.channel()));
    assert!(!workspace.open_remote_channel(other.team(), other.channel(), &mut toast));
    assert!(workspace.take_remote_channel_messages_request().is_none());
}
