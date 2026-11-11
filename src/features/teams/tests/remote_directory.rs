use super::super::{TeamsAction, TeamsState};
use super::{remote_directory, remote_messages, remote_post_target, remote_replies};

#[test]
fn remote_directory_never_enters_the_local_teams_snapshot() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, _, _) = remote_directory("Engineering", "Planning");

    state.replace_remote_directory(directory);

    assert!(!state.remote.is_empty());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_selection_is_opaque_and_clear_falls_back_to_the_local_channel() {
    let mut state = TeamsState::default();
    let local_selection = state.selected_channel.clone();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);

    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    state.on_remote_channel_selected();
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target,
        channel_target,
        "Remote channel post",
    )));
    assert!(state.remote.has_selection());
    assert_eq!(state.selected_channel, local_selection);
    assert!(!state.has_keyboard_modal());

    state.clear_remote_directory();

    assert!(state.remote.is_empty());
    assert!(!state.remote.has_selection());
    assert_eq!(state.selected_channel, local_selection);
}

#[test]
fn remote_channel_messages_never_enter_the_local_teams_snapshot() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );

    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target,
        channel_target,
        "Remote delivery update",
    )));

    assert_eq!(state.remote.loaded_post_count(), 1);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());

    state.clear_remote_channel_messages();

    assert!(state.remote.is_loading_messages());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_replies_never_enter_the_local_teams_snapshot() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Remote reply thread");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Remote reply thread",
    )));
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
    ));
    assert!(state.is_loading_remote_channel_replies());

    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target,
        channel_target,
        post_target,
        "A session-only Microsoft 365 reply.",
    )));

    assert_eq!(state.remote_channel_reply_count(), 1);
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_post_selection_action_retains_all_opaque_targets() {
    let mut state = TeamsState::default();
    let local_channel = state.selected_channel.clone();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Opaque post target");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Opaque post target",
    )));

    assert_eq!(
        state.remote_channel_post_selection_action(post_target.clone()),
        Some(TeamsAction::RemoteChannelPostSelected {
            team: team_target,
            channel: channel_target,
            post: post_target,
        })
    );
    assert!(state.is_loading_remote_channel_replies());
    assert_eq!(state.selected_channel, local_channel);
    assert!(state.active_thread.is_none());
    assert!(!state.has_keyboard_modal());
}
