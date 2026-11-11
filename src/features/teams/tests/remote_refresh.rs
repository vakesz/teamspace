use super::super::TeamsState;
use super::{
    remote_directory, remote_messages, remote_messages_with_like, remote_post_target,
    remote_replies,
};
use crate::shared::reaction::ReactionKind;

fn ready_channel() -> (
    TeamsState,
    crate::integrations::ProviderResourceRef,
    crate::integrations::ProviderResourceRef,
    crate::integrations::ProviderResourceRef,
) {
    let mut state = TeamsState::default();
    let (directory, team, channel) = remote_directory("Engineering", "Planning");
    let post = remote_post_target("Root post");
    state.replace_remote_directory(directory);
    assert!(state.remote.select_for_test(team.clone(), channel.clone()));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team.clone(),
        channel.clone(),
        "Root post",
    )));
    (state, team, channel, post)
}

#[test]
fn a_posts_refresh_keeps_ready_rows_and_returns_the_exact_targets() {
    let (mut state, team, channel, _) = ready_channel();
    assert_eq!(state.remote_channel_message_count(), 1);

    let targets = state.begin_remote_channel_messages_refresh();
    assert_eq!(targets, Some((team, channel)));
    assert_eq!(state.remote_channel_message_count(), 1, "rows stay visible");
    // Repeating is harmless and still keeps the rows.
    assert!(state.begin_remote_channel_messages_refresh().is_some());
    assert_eq!(state.remote_channel_message_count(), 1);
}

#[test]
fn a_posts_refresh_needs_a_ready_feed() {
    let mut state = TeamsState::default();
    assert!(state.begin_remote_channel_messages_refresh().is_none());
    let (directory, team, channel) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(state.remote.select_for_test(team, channel));
    // Selected but still Loading: the selection read is already running.
    assert!(state.begin_remote_channel_messages_refresh().is_none());
}

#[test]
fn a_replies_refresh_keeps_ready_rows_and_needs_a_ready_reply_feed() {
    let (mut state, team, channel, post) = ready_channel();
    assert!(state.begin_remote_channel_replies_refresh().is_none());
    assert!(
        state
            .remote_channel_post_selection_action(post.clone())
            .is_some()
    );
    assert!(
        state.begin_remote_channel_replies_refresh().is_none(),
        "replies still loading"
    );
    assert!(state.replace_remote_channel_replies(remote_replies(
        team.clone(),
        channel.clone(),
        post.clone(),
        "A reply",
    )));
    assert_eq!(state.remote_channel_reply_count(), 1);

    assert_eq!(
        state.begin_remote_channel_replies_refresh(),
        Some((team, channel, post))
    );
    assert_eq!(state.remote_channel_reply_count(), 1, "rows stay visible");
    assert!(!state.is_loading_remote_channel_replies());
}

#[test]
fn a_refresh_leaves_a_reaction_gate_for_the_next_exact_feed() {
    let (mut state, team, channel, _) = ready_channel();
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team.clone(),
            channel.clone(),
            "Root post",
            0,
            false,
        ))
    );
    let post = remote_post_target("Root post");
    assert!(
        state
            .remote
            .queue_channel_post_reaction(post.clone(), ReactionKind::Like, true)
            .is_some()
    );
    assert!(state.begin_remote_channel_messages_refresh().is_some());
    // A feed that does not yet show the requested state keeps the gate:
    // refreshing must not unlock a duplicate click.
    assert!(
        state.replace_remote_channel_messages(remote_messages_with_like(
            team,
            channel,
            "Root post",
            0,
            false,
        ))
    );
    assert!(
        state
            .remote
            .queue_channel_post_reaction(post, ReactionKind::Like, true)
            .is_none(),
        "the bound gate stays until the provider shows the requested state"
    );
}
