use super::super::TeamsState;
use super::{
    archived_remote_directory, remote_channel_post_action, remote_channel_reply_action,
    remote_directory, remote_messages, remote_messages_for_posts, remote_post_target,
    remote_two_channel_directory, render_selected_channel,
};
use crate::features::threads::ThreadStore;
use crate::integrations::WorkspaceIntentId;

#[test]
fn remote_root_post_command_binds_opaque_targets_without_local_mutation() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    let stored_before = state.stored();

    assert!(
        state
            .remote
            .set_channel_post_draft_for_test("  Exact remote body.  \n")
    );
    let (submission, team, channel, body) = remote_channel_post_action(&mut state);

    assert_eq!(team, team_target);
    assert_eq!(channel, channel_target);
    assert_eq!(body, "  Exact remote body.  \n");
    assert!(
        state.bind_remote_channel_post_submission(submission, WorkspaceIntentId::for_test(401),)
    );
    assert!(
        !state.bind_remote_channel_post_submission(submission, WorkspaceIntentId::for_test(402),)
    );
    assert!(state.remote.has_pending_channel_post_for_test());
    assert_eq!(state.remote_channel_message_count(), 0);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_post_validation_never_creates_a_local_or_pending_post() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target, channel_target));
    let stored_before = state.stored();

    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::Empty)
    ));
    assert!(state.remote.set_channel_post_draft_for_test("has\0null"));
    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::NullByte)
    ));
    let oversized = "x".repeat(64 * 1024 + 1);
    assert!(state.remote.set_channel_post_draft_for_test(&oversized));
    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::TooLong)
    ));

    assert!(!state.remote.has_pending_channel_post_for_test());
    assert_eq!(state.remote_channel_message_count(), 0);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_post_201_clears_only_the_session_composer() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target, channel_target));
    let stored_before = state.stored();
    assert!(
        state
            .remote
            .set_channel_post_draft_for_test("Delivered remotely.")
    );
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(403);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    assert!(!state.remote_channel_post_delivered(WorkspaceIntentId::for_test(404)));
    assert!(state.remote_channel_post_delivered(intent));
    assert!(!state.remote.has_pending_channel_post_for_test());
    assert_eq!(state.remote.channel_post_draft_for_test(), Some(""));
    assert_eq!(state.remote_channel_message_count(), 0);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_post_known_retry_keeps_the_draft_locked() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target, channel_target));
    assert!(
        state
            .remote
            .set_channel_post_draft_for_test("Retry only when safe.")
    );
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(405);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    assert!(state.remote_channel_post_retry_scheduled(intent));
    assert!(!state.remote_channel_post_retry_scheduled(WorkspaceIntentId::for_test(406)));
    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::Pending)
    ));
    assert!(state.remote.has_pending_channel_post_for_test());
}

#[test]
fn remote_root_post_definite_failure_restores_the_exact_draft() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target, channel_target));
    let draft = "  Preserve this exact body.\n";
    assert!(state.remote.set_channel_post_draft_for_test(draft));
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(407);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    assert!(!state.remote_channel_post_failed(WorkspaceIntentId::for_test(408)));
    assert!(state.remote_channel_post_failed(intent));
    assert!(!state.remote.has_pending_channel_post_for_test());
    assert_eq!(state.remote.channel_post_draft_for_test(), Some(draft));
    let (_, _, _, resubmitted_body) = remote_channel_post_action(&mut state);
    assert_eq!(resubmitted_body, draft);
}

#[test]
fn remote_root_post_ambiguous_delivery_requires_explicit_manual_review() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target, channel_target));
    let draft = "Review before retrying.";
    assert!(state.remote.set_channel_post_draft_for_test(draft));
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(409);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    assert!(state.remote_channel_post_delivery_uncertain(intent));
    assert!(state.remote.has_unknown_channel_post_for_test());
    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::Pending)
    ));
    assert!(state.remote.make_uncertain_channel_post_editable());
    assert!(!state.remote.has_unknown_channel_post_for_test());
    assert_eq!(state.remote.channel_post_draft_for_test(), Some(draft));
    let (_, _, _, reviewed_body) = remote_channel_post_action(&mut state);
    assert_eq!(reviewed_body, draft);
}

#[test]
fn transport_loss_clear_retains_a_remote_post_until_its_outcome_is_mapped() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    let draft = "Retain this while the worker result is still possible.";
    assert!(state.remote.set_channel_post_draft_for_test(draft));
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(412);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    state.clear_remote_directory_preserving_delivery_recovery();
    assert!(state.remote.is_empty());
    assert!(!state.remote.has_selection());
    assert!(state.remote.has_pending_channel_post_for_test());
    assert!(state.remote_channel_post_delivery_uncertain(intent));
    assert!(state.remote.has_unknown_channel_post_for_test());

    let (directory, replacement_team, replacement_channel) =
        remote_directory("Engineering", "Planning");
    assert_eq!(replacement_team, team_target);
    assert_eq!(replacement_channel, channel_target);
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(replacement_team, replacement_channel));
    assert!(state.remote.make_uncertain_channel_post_editable());
    assert_eq!(state.remote.channel_post_draft_for_test(), Some(draft));

    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let explicit_reset_intent = WorkspaceIntentId::for_test(413);
    assert!(state.bind_remote_channel_post_submission(submission, explicit_reset_intent));
    state.clear_remote_directory();
    assert!(!state.remote_channel_post_delivery_uncertain(explicit_reset_intent));
    assert!(!state.remote.has_pending_channel_post_for_test());
}

#[test]
fn remote_root_post_failure_survives_selection_change_until_user_returns() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    let (directory, team_target, planning, delivery) = remote_two_channel_directory();
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), planning.clone()));
    let draft = "  Planning draft restored after navigation.  ";
    assert!(state.remote.set_channel_post_draft_for_test(draft));
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let intent = WorkspaceIntentId::for_test(410);
    assert!(state.bind_remote_channel_post_submission(submission, intent));

    assert!(state.select_remote_channel_for_test(team_target.clone(), delivery.clone()));
    assert!(
        !state
            .remote
            .set_channel_post_draft_for_test("Delivery must stay locked.")
    );
    assert!(!state.remote_channel_post_failed(WorkspaceIntentId::for_test(411)));
    assert!(state.remote_channel_post_failed(intent));
    // Rendering B performs the normal selected-composer transition. It must
    // not discard the just-restored Planning text while making B usable.
    render_selected_channel(&mut state, &mut discussions);
    assert!(
        state
            .remote
            .set_channel_post_draft_for_test("Delivery is usable.")
    );

    assert!(state.select_remote_channel_for_test(team_target, planning));
    render_selected_channel(&mut state, &mut discussions);
    assert_eq!(state.remote.channel_post_draft_for_test(), Some(draft));
}

#[test]
fn remote_root_reply_command_binds_exact_targets_without_local_mutation() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reply command target");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Reply command target",
    )));
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
    ));
    let stored_before = state.stored();

    assert!(state.set_remote_channel_reply_draft_for_test("  Exact reply body.  \n"));
    let (submission, team, channel, post, body) = remote_channel_reply_action(&mut state);

    assert_eq!(team, team_target);
    assert_eq!(channel, channel_target);
    assert_eq!(post, post_target);
    assert_eq!(body, "  Exact reply body.  \n");
    assert!(
        state.bind_remote_channel_reply_submission(submission, WorkspaceIntentId::for_test(501),)
    );
    assert!(
        !state.bind_remote_channel_reply_submission(submission, WorkspaceIntentId::for_test(502),)
    );
    assert!(state.remote.has_pending_channel_reply_for_test());
    assert_eq!(state.remote_channel_reply_count(), 0);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_reply_validation_rejects_invalid_text_without_local_state() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reply validation target");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Reply validation target",
    )));
    assert!(state.select_remote_channel_post_for_test(team_target, channel_target, post_target));
    let stored_before = state.stored();

    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::Empty)
    ));
    assert!(state.set_remote_channel_reply_draft_for_test("has\0null"));
    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::NullByte)
    ));
    assert!(state.set_remote_channel_reply_draft_for_test(&"x".repeat(64 * 1024 + 1)));
    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::TooLong)
    ));

    assert!(!state.remote.has_pending_channel_reply_for_test());
    assert_eq!(state.remote_channel_reply_count(), 0);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_root_reply_lifecycle_preserves_exact_text_until_explicit_review() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Reply lifecycle target");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Reply lifecycle target",
    )));
    assert!(state.select_remote_channel_post_for_test(team_target, channel_target, post_target));
    let stored_before = state.stored();

    assert!(state.set_remote_channel_reply_draft_for_test("Delivered only after 201."));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let delivered = WorkspaceIntentId::for_test(503);
    assert!(state.bind_remote_channel_reply_submission(submission, delivered));
    assert!(state.remote_channel_reply_retry_scheduled(delivered));
    assert!(!state.remote_channel_reply_retry_scheduled(WorkspaceIntentId::for_test(504)));
    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::Pending)
    ));
    assert!(state.remote_channel_reply_delivered(delivered));
    assert!(!state.remote.has_pending_channel_reply_for_test());
    assert_eq!(state.remote.channel_reply_draft_for_test(), Some(""));

    let rejected = "  Preserve this rejected reply exactly.\n";
    assert!(state.set_remote_channel_reply_draft_for_test(rejected));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let rejected_intent = WorkspaceIntentId::for_test(505);
    assert!(state.bind_remote_channel_reply_submission(submission, rejected_intent));
    assert!(!state.remote_channel_reply_failed(WorkspaceIntentId::for_test(506)));
    assert!(state.remote_channel_reply_failed(rejected_intent));
    assert_eq!(state.remote.channel_reply_draft_for_test(), Some(rejected));
    let (submission, _, _, _, resubmitted_body) = remote_channel_reply_action(&mut state);
    assert_eq!(resubmitted_body, rejected);
    let reviewed_rejection = WorkspaceIntentId::for_test(507);
    assert!(state.bind_remote_channel_reply_submission(submission, reviewed_rejection));
    assert!(state.remote_channel_reply_failed(reviewed_rejection));

    let uncertain = "Review this reply before deciding.";
    assert!(state.set_remote_channel_reply_draft_for_test(uncertain));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let unknown_intent = WorkspaceIntentId::for_test(508);
    assert!(state.bind_remote_channel_reply_submission(submission, unknown_intent));
    assert!(state.remote_channel_reply_delivery_uncertain(unknown_intent));
    assert!(state.remote.has_unknown_channel_reply_for_test());
    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::Pending)
    ));
    assert!(state.remote.make_uncertain_channel_reply_editable());
    assert!(!state.remote.has_unknown_channel_reply_for_test());
    assert_eq!(state.remote.channel_reply_draft_for_test(), Some(uncertain));
    let (_, _, _, _, reviewed_body) = remote_channel_reply_action(&mut state);
    assert_eq!(reviewed_body, uncertain);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_reply_drafts_are_isolated_by_selected_root_across_navigation_and_render() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let first_post = remote_post_target("Reply draft A");
    let second_post = remote_post_target("Reply draft B");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(
        state.replace_remote_channel_messages(remote_messages_for_posts(
            team_target.clone(),
            channel_target.clone(),
            &["Reply draft A", "Reply draft B"],
        ))
    );
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        first_post.clone(),
    ));
    assert!(state.set_remote_channel_reply_draft_for_test("Draft for root A."));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let first_intent = WorkspaceIntentId::for_test(513);
    assert!(state.bind_remote_channel_reply_submission(submission, first_intent));

    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        second_post.clone(),
    ));
    assert!(
        !state.set_remote_channel_reply_draft_for_test("Root B stays locked until A resolves.")
    );
    assert!(state.remote_channel_reply_failed(first_intent));
    render_selected_channel(&mut state, &mut discussions);
    assert!(state.set_remote_channel_reply_draft_for_test("Draft for root B."));

    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        first_post,
    ));
    render_selected_channel(&mut state, &mut discussions);
    assert_eq!(
        state.remote.channel_reply_draft_for_test(),
        Some("Draft for root A.")
    );

    assert!(state.select_remote_channel_post_for_test(team_target, channel_target, second_post));
    render_selected_channel(&mut state, &mut discussions);
    assert_eq!(
        state.remote.channel_reply_draft_for_test(),
        Some("Draft for root B.")
    );
}

#[test]
fn remote_root_posts_and_replies_share_one_active_delivery_gate() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Shared delivery gate");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Shared delivery gate",
    )));
    assert!(state.select_remote_channel_post_for_test(team_target, channel_target, post_target,));

    assert!(
        state
            .remote
            .set_channel_post_draft_for_test("Root post first.")
    );
    let (submission, _, _, _) = remote_channel_post_action(&mut state);
    let root_intent = WorkspaceIntentId::for_test(509);
    assert!(state.bind_remote_channel_post_submission(submission, root_intent));
    assert!(!state.set_remote_channel_reply_draft_for_test("Blocked by root post."));
    assert!(matches!(
        state.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::Pending)
    ));
    assert!(state.remote_channel_post_failed(root_intent));

    assert!(state.set_remote_channel_reply_draft_for_test("Reply second."));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let reply_intent = WorkspaceIntentId::for_test(510);
    assert!(state.bind_remote_channel_reply_submission(submission, reply_intent));
    assert!(
        !state
            .remote
            .set_channel_post_draft_for_test("Blocked by reply.")
    );
    assert!(matches!(
        state.remote_channel_post_submission_action(),
        Err(super::super::remote::RemoteChannelPostError::Pending)
    ));
    assert!(state.remote_channel_reply_failed(reply_intent));
}

#[test]
fn remote_reply_archive_and_transport_loss_guards_preserve_or_clear_recovery_correctly() {
    let mut archived = TeamsState::default();
    let (directory, team_target, channel_target) = archived_remote_directory();
    let post_target = remote_post_target("Archived reply target");
    archived.replace_remote_directory(directory);
    assert!(archived.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(archived.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Archived reply target",
    )));
    assert!(archived.select_remote_channel_post_for_test(team_target, channel_target, post_target));
    let archived_stored = archived.stored();
    assert!(archived.set_remote_channel_reply_draft_for_test("Not allowed."));
    assert!(matches!(
        archived.remote_channel_reply_submission_action(),
        Err(super::super::remote_delivery::RemoteChannelDeliveryError::Archived)
    ));
    assert!(!archived.remote.has_pending_channel_reply_for_test());
    assert_eq!(archived.stored(), archived_stored);

    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Transport reply target");
    state.replace_remote_directory(directory);
    assert!(state.select_remote_channel_for_test(team_target.clone(), channel_target.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Transport reply target",
    )));
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
    ));
    let draft = "Retain this reply while the worker may still report an outcome.";
    assert!(state.set_remote_channel_reply_draft_for_test(draft));
    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let intent = WorkspaceIntentId::for_test(511);
    assert!(state.bind_remote_channel_reply_submission(submission, intent));

    state.clear_remote_directory_preserving_delivery_recovery();
    assert!(state.remote.is_empty());
    assert!(state.remote.has_pending_channel_reply_for_test());
    assert!(state.remote_channel_reply_delivery_uncertain(intent));
    assert!(state.remote.has_unknown_channel_reply_for_test());

    let (directory, replacement_team, replacement_channel) =
        remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .select_remote_channel_for_test(replacement_team.clone(), replacement_channel.clone(),)
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        replacement_team.clone(),
        replacement_channel.clone(),
        "Transport reply target",
    )));
    assert!(state.select_remote_channel_post_for_test(
        replacement_team,
        replacement_channel,
        post_target,
    ));
    assert!(state.remote.make_uncertain_channel_reply_editable());
    assert_eq!(state.remote.channel_reply_draft_for_test(), Some(draft));

    let (submission, _, _, _, _) = remote_channel_reply_action(&mut state);
    let reset_intent = WorkspaceIntentId::for_test(512);
    assert!(state.bind_remote_channel_reply_submission(submission, reset_intent));
    state.clear_remote_directory();
    assert!(!state.remote_channel_reply_delivery_uncertain(reset_intent));
    assert!(!state.remote.has_pending_channel_reply_for_test());
}
