use super::super::{
    RemoteChannelDirectory, RemoteChannelFile, RemoteChannelFilesDirectory,
    RemoteChannelMessagesDirectory, RemoteChannelRepliesDirectory, RemoteTeamDirectory,
    RemoteTeamsDirectory, TeamsAction, TeamsState,
};
use super::{
    remote_directory, remote_files, remote_messages, remote_messages_for_posts, remote_post_target,
    remote_replies, render_selected_channel,
};
use crate::features::threads::ThreadStore;
use crate::integrations::{ProviderBrowserUrl, ProviderResourceRef};
use crate::ui::toast::ToastState;
use eframe::egui;

#[test]
fn remote_channel_reply_response_for_another_selected_post_is_ignored() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let first_post = remote_post_target("First root post");
    let second_post = remote_post_target("Second root post");
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
            &["First root post", "Second root post"],
        ))
    );
    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(
        state
            .remote_channel_post_selection_action(second_post.clone())
            .is_some()
    );

    assert!(!state.replace_remote_channel_replies(remote_replies(
        team_target,
        channel_target,
        first_post,
        "This reply belongs to the stale root post.",
    )));

    assert!(state.is_loading_remote_channel_replies());
    assert_eq!(state.remote_channel_reply_count(), 0);
}

#[test]
fn remote_channel_reply_response_for_another_selected_channel_is_ignored() {
    let mut state = TeamsState::default();
    let (planning_directory, team_target, planning_channel) =
        remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Planning root post");
    state.replace_remote_directory(planning_directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), planning_channel.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        planning_channel.clone(),
        "Planning root post",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );

    let (delivery_directory, replacement_team, delivery_channel) =
        remote_directory("Engineering", "Delivery");
    assert_eq!(replacement_team, team_target);
    state.replace_remote_directory(delivery_directory);
    assert!(
        state
            .remote
            .select_for_test(replacement_team, delivery_channel)
    );

    assert!(!state.replace_remote_channel_replies(remote_replies(
        team_target,
        planning_channel,
        post_target,
        "This reply belongs to the previously selected channel.",
    )));
    assert!(!state.is_loading_remote_channel_replies());
    assert_eq!(state.remote_channel_reply_count(), 0);
}

#[test]
fn remote_channel_reply_selection_clears_on_post_channel_local_and_remote_changes() {
    let mut state = TeamsState::default();
    let team_target = ProviderResourceRef::local_preview("test-team:engineering");
    let planning = ProviderResourceRef::local_preview("test-channel:engineering:planning");
    let delivery = ProviderResourceRef::local_preview("test-channel:engineering:delivery");
    state.replace_remote_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team_target.clone(),
            "Engineering".into(),
            String::new(),
            vec![
                RemoteChannelDirectory::microsoft365(
                    planning.clone(),
                    "Planning".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
                RemoteChannelDirectory::microsoft365(
                    delivery.clone(),
                    "Delivery".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
            ],
        ),
    ]));
    let first_post = remote_post_target("First root post");
    let second_post = remote_post_target("Second root post");
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), planning.clone())
    );
    assert!(
        state.replace_remote_channel_messages(remote_messages_for_posts(
            team_target.clone(),
            planning.clone(),
            &["First root post", "Second root post"],
        ))
    );
    assert!(
        state
            .remote_channel_post_selection_action(first_post.clone())
            .is_some()
    );
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        planning.clone(),
        first_post,
        "Visible before selection changes.",
    )));

    assert!(
        state
            .remote_channel_post_selection_action(second_post)
            .is_some()
    );
    assert!(state.is_loading_remote_channel_replies());
    assert_eq!(state.remote_channel_reply_count(), 0);
    assert!(state.take_remote_channel_replies_clear_requested());

    assert!(state.remote.select_for_test(team_target.clone(), delivery));
    assert!(!state.is_loading_remote_channel_replies());
    assert_eq!(state.remote_channel_reply_count(), 0);
    assert!(state.take_remote_channel_replies_clear_requested());

    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), planning.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        planning.clone(),
        "Local selection root post",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(remote_post_target("Local selection root post"))
            .is_some()
    );
    state.select_team_channel("Bike Commuters · General");
    assert!(!state.remote.has_selection());
    assert!(!state.is_loading_remote_channel_replies());
    assert!(state.take_remote_channel_replies_clear_requested());

    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), planning.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        planning,
        "Remote clear root post",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(remote_post_target("Remote clear root post"))
            .is_some()
    );
    state.clear_remote_directory();
    assert!(state.remote.is_empty());
    assert!(!state.is_loading_remote_channel_replies());
    assert_eq!(state.remote_channel_reply_count(), 0);
    assert!(state.take_remote_channel_replies_clear_requested());
}

#[test]
fn remote_channel_reply_access_denial_is_session_only() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Unreadable root post");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Unreadable root post",
    )));
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
    ));

    assert!(
        state.replace_remote_channel_replies(RemoteChannelRepliesDirectory::access_denied(
            team_target,
            channel_target,
            post_target
        ),)
    );

    assert!(state.has_remote_channel_reply_access_denied());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_reply_refresh_reuses_the_selected_targets_without_cancellation() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Retryable root post");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(!state.has_selected_remote_channel_reply_for(&team_target, &channel_target));
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Retryable root post",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(state.has_selected_remote_channel_reply_for(&team_target, &channel_target));
    assert!(!state.has_selected_remote_channel_reply_for(
        &team_target,
        &ProviderResourceRef::local_preview("different-channel"),
    ));
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
        "A ready reply projection.",
    )));
    assert!(!state.take_remote_channel_replies_clear_requested());

    let expected_action = Some(TeamsAction::RemoteChannelPostSelected {
        team: team_target.clone(),
        channel: channel_target.clone(),
        post: post_target.clone(),
    });
    assert_eq!(state.remote_channel_reply_refresh_action(), expected_action);
    assert!(state.is_loading_remote_channel_replies());
    assert!(state.has_selected_remote_channel_reply_for(&team_target, &channel_target));
    assert!(!state.take_remote_channel_replies_clear_requested());
    // A transport error can leave the feature visibly loading after its
    // worker finishes without a projection. Retrying that state reuses the
    // same read intent; the app-owned controller deduplicates an active or
    // queued same-target request.
    assert_eq!(state.remote_channel_reply_refresh_action(), expected_action);
    assert!(!state.take_remote_channel_replies_clear_requested());

    assert!(
        state.replace_remote_channel_replies(RemoteChannelRepliesDirectory::access_denied(
            team_target.clone(),
            channel_target.clone(),
            post_target.clone(),
        ),)
    );
    assert!(state.has_remote_channel_reply_access_denied());
    assert_eq!(state.remote_channel_reply_refresh_action(), expected_action);
    assert!(state.is_loading_remote_channel_replies());
    assert!(!state.take_remote_channel_replies_clear_requested());
}

#[test]
fn remote_channel_files_are_session_only_and_preserve_the_posts_and_replies_surface() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Files do not replace replies");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Files do not replace replies",
    )));
    assert!(
        state
            .remote_channel_post_selection_action(post_target.clone())
            .is_some()
    );
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target.clone(),
        channel_target.clone(),
        post_target,
        "A reply remains visible after switching to Files.",
    )));

    assert_eq!(
        state.remote_channel_files_selection_action(),
        Some(TeamsAction::RemoteChannelFilesSelected {
            team: team_target.clone(),
            channel: channel_target.clone(),
        })
    );
    assert!(state.remote.is_showing_files());
    assert!(state.is_loading_remote_channel_files());
    assert_eq!(state.remote_channel_message_count(), 1);
    assert_eq!(state.remote_channel_reply_count(), 1);
    assert!(!state.take_remote_channel_files_clear_requested());

    assert!(state.replace_remote_channel_files(remote_files(
        team_target,
        channel_target,
        "Sprint-plan.docx",
    )));
    assert_eq!(state.remote_channel_file_count(), 1);
    assert_eq!(state.remote_channel_message_count(), 1);
    assert_eq!(state.remote_channel_reply_count(), 1);
    let mut discussions = ThreadStore::default();
    render_selected_channel(&mut state, &mut discussions);
    assert!(state.take_remote_channel_file_open_links().is_empty());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());

    state.remote_channel_posts_surface_action();
    assert!(!state.remote.is_showing_files());
    assert_eq!(state.remote_channel_file_count(), 1);
    assert_eq!(state.remote_channel_reply_count(), 1);
    assert_eq!(state.remote_channel_files_selection_action(), None);
    assert!(state.remote.is_showing_files());
}

#[test]
fn remote_channel_file_browser_handoff_is_explicit_target_bound_and_session_only() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let file_target = ProviderResourceRef::local_preview("test-channel-file:browser-current");
    let stale_target = ProviderResourceRef::local_preview("test-channel-file:browser-stale");
    let destination = "https://contoso.sharepoint.com/:x:/r/opaque-channel-file";
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());
    assert!(
        state.replace_remote_channel_files(RemoteChannelFilesDirectory::microsoft365(
            team_target,
            channel_target,
            vec![RemoteChannelFile::microsoft365(
                file_target.clone(),
                "Sprint-plan.docx".into(),
                "2026-11-06T10:40:00Z".into(),
                Some(42),
                "File".into(),
                Some(ProviderBrowserUrl::for_test(destination)),
            )],
        ))
    );

    // Only a target in the current selected channel-file projection can hand
    // a typed browser destination to the app shell. A stale ID never turns a
    // visible name or another channel's item into an external navigation.
    assert!(!state.request_remote_channel_file_open_for_test(stale_target));
    assert!(state.take_remote_channel_file_open_links().is_empty());
    assert!(state.request_remote_channel_file_open_for_test(file_target));
    assert_eq!(
        state.take_remote_channel_file_open_links(),
        vec![ProviderBrowserUrl::for_test(destination)]
    );
    assert!(state.take_remote_channel_file_open_links().is_empty());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_file_handoffs_drop_on_projection_replacement_and_clear() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let file_target = ProviderResourceRef::local_preview("test-channel-file:browser-drop");
    let destination = "https://contoso.sharepoint.com/:x:/r/opaque-channel-file-drop";
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());
    let directory_with_link = || {
        RemoteChannelFilesDirectory::microsoft365(
            team_target.clone(),
            channel_target.clone(),
            vec![RemoteChannelFile::microsoft365(
                file_target.clone(),
                "Sprint-plan.docx".into(),
                "2026-11-06T10:40:00Z".into(),
                Some(42),
                "File".into(),
                Some(ProviderBrowserUrl::for_test(destination)),
            )],
        )
    };
    assert!(state.replace_remote_channel_files(directory_with_link()));
    assert!(state.request_remote_channel_file_open_for_test(file_target.clone()));

    // A fresh target projection may remove or reassign the item, so its
    // queued handoff cannot survive the replacement.
    assert!(state.replace_remote_channel_files(remote_files(
        team_target.clone(),
        channel_target.clone(),
        "Replacement-without-browser-link.docx",
    )));
    assert!(state.take_remote_channel_file_open_links().is_empty());

    assert!(state.replace_remote_channel_files(directory_with_link()));
    assert!(state.request_remote_channel_file_open_for_test(file_target));
    state.clear_remote_directory();
    assert!(state.take_remote_channel_file_open_links().is_empty());
}

#[test]
fn remote_channel_files_reject_a_late_different_channel_response() {
    let mut state = TeamsState::default();
    let (planning_directory, team_target, planning_channel) =
        remote_directory("Engineering", "Planning");
    state.replace_remote_directory(planning_directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), planning_channel.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());

    let (delivery_directory, replacement_team, delivery_channel) =
        remote_directory("Engineering", "Delivery");
    assert_eq!(replacement_team, team_target);
    state.replace_remote_directory(delivery_directory);
    assert!(
        state
            .remote
            .select_for_test(replacement_team.clone(), delivery_channel.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());

    assert!(!state.replace_remote_channel_files(remote_files(
        team_target,
        planning_channel,
        "Stale planning file.txt",
    )));
    assert!(state.is_loading_remote_channel_files());
    assert_eq!(state.remote_channel_file_count(), 0);
    assert_eq!(
        state.remote_channel_files_selection_action(),
        None,
        "the same selected channel must not enqueue a duplicate initial read"
    );
    assert_eq!(
        state.remote_channel_files_refresh_action(),
        Some(TeamsAction::RemoteChannelFilesSelected {
            team: replacement_team,
            channel: delivery_channel,
        })
    );
}

#[test]
fn remote_channel_files_clear_on_local_selection_and_directory_reset() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());
    assert!(state.replace_remote_channel_files(remote_files(
        team_target.clone(),
        channel_target.clone(),
        "Local-selection cleanup.txt",
    )));

    state.select_team_channel("Bike Commuters · General");
    assert!(!state.remote.has_selection());
    assert!(!state.remote.is_showing_files());
    assert_eq!(state.remote_channel_file_count(), 0);
    assert!(state.take_remote_channel_files_clear_requested());
    assert!(!state.take_remote_channel_files_clear_requested());

    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    assert!(state.remote_channel_files_selection_action().is_some());
    state.clear_remote_directory();
    assert!(state.remote.is_empty());
    assert_eq!(state.remote_channel_file_count(), 0);
    assert!(state.take_remote_channel_files_clear_requested());
    assert!(!state.replace_remote_channel_files(remote_files(
        team_target,
        channel_target,
        "Stale after directory reset.txt",
    )));
}

#[test]
fn remote_channel_files_access_denial_and_retry_remain_read_only() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    let expected_action = Some(TeamsAction::RemoteChannelFilesSelected {
        team: team_target.clone(),
        channel: channel_target.clone(),
    });
    assert_eq!(
        state.remote_channel_files_selection_action(),
        expected_action
    );
    assert!(
        state.replace_remote_channel_files(RemoteChannelFilesDirectory::access_denied(
            team_target,
            channel_target,
        ))
    );
    assert!(state.has_remote_channel_file_access_denied());
    assert_eq!(state.remote_channel_files_refresh_action(), expected_action);
    assert!(state.is_loading_remote_channel_files());
    // A failed worker may leave the feature visibly Loading without a
    // projection. Repeating the exact read remains safe because the
    // app-owned controller deduplicates active/queued same-target work.
    assert_eq!(state.remote_channel_files_refresh_action(), expected_action);
    assert!(!state.take_remote_channel_files_clear_requested());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_message_response_for_an_older_selection_is_ignored() {
    let mut state = TeamsState::default();
    let team_target = ProviderResourceRef::local_preview("test-team:engineering");
    let first_channel = ProviderResourceRef::local_preview("test-channel:engineering:planning");
    let second_channel = ProviderResourceRef::local_preview("test-channel:engineering:delivery");
    state.replace_remote_directory(RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            team_target.clone(),
            "Engineering".into(),
            String::new(),
            vec![
                RemoteChannelDirectory::microsoft365(
                    first_channel.clone(),
                    "Planning".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
                RemoteChannelDirectory::microsoft365(
                    second_channel.clone(),
                    "Delivery".into(),
                    String::new(),
                    "Standard".into(),
                    false,
                ),
            ],
        ),
    ]));
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), first_channel.clone())
    );
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), second_channel.clone())
    );

    assert!(!state.replace_remote_channel_messages(remote_messages(
        team_target,
        first_channel,
        "Stale planning post",
    )));

    assert!(state.remote.is_loading_messages());
    assert_eq!(state.remote.loaded_post_count(), 0);
}

#[test]
fn remote_channel_selection_action_retains_opaque_targets() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    state.on_remote_channel_selected();

    assert_eq!(
        state.remote_channel_selection_action(),
        Some(TeamsAction::RemoteChannelSelected {
            team: team_target,
            channel: channel_target,
        })
    );
}

#[test]
fn remote_channel_access_denial_is_session_only() {
    let mut state = TeamsState::default();
    let stored_before = state.stored();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );

    assert!(
        state.replace_remote_channel_messages(RemoteChannelMessagesDirectory::access_denied(
            team_target,
            channel_target
        ),)
    );

    assert!(state.remote.has_message_access_denied());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn remote_channel_refresh_reuses_the_selected_target_without_persisting() {
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
        team_target.clone(),
        channel_target.clone(),
        "A post before refresh",
    )));

    // The page's Refresh posts control delegates to this same action even
    // when the user is already viewing the channel. It keeps the opaque
    // selection stable and leaves all durable local Teams data untouched.
    assert_eq!(
        state.remote_channel_selection_action(),
        Some(TeamsAction::RemoteChannelSelected {
            team: team_target,
            channel: channel_target,
        })
    );
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn replacing_remote_directory_preserves_selection_by_target_not_display_name() {
    let mut state = TeamsState::default();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );

    let replacement = RemoteTeamsDirectory::microsoft365(vec![
        RemoteTeamDirectory::microsoft365(
            ProviderResourceRef::local_preview("test-team:other"),
            "Engineering".into(),
            "Same display name, different provider target.".into(),
            vec![],
        ),
        RemoteTeamDirectory::microsoft365(
            team_target.clone(),
            "Renamed engineering team".into(),
            "Same provider target.".into(),
            vec![RemoteChannelDirectory::microsoft365(
                channel_target,
                "Renamed planning channel".into(),
                String::new(),
                "Private".into(),
                false,
            )],
        ),
    ]);

    state.replace_remote_directory(replacement);

    assert!(state.remote.has_selection());
    assert_eq!(state.selected_channel, "Bike Commuters · General");
    assert_eq!(
        state.stored().selected_channel.as_deref(),
        Some("Bike Commuters · General")
    );
}

#[test]
fn remote_channel_page_short_circuits_the_local_composer_and_actions() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Keep my local preview draft.".into();
    let (directory, team_target, channel_target) = remote_directory("Engineering", "Planning");
    let post_target = remote_post_target("Read-only remote post");
    state.replace_remote_directory(directory);
    assert!(
        state
            .remote
            .select_for_test(team_target.clone(), channel_target.clone())
    );
    state.on_remote_channel_selected();
    assert!(state.replace_remote_channel_messages(remote_messages(
        team_target.clone(),
        channel_target.clone(),
        "Read-only remote post",
    )));
    assert!(state.select_remote_channel_post_for_test(
        team_target.clone(),
        channel_target.clone(),
        post_target.clone(),
    ));
    assert!(state.replace_remote_channel_replies(remote_replies(
        team_target,
        channel_target,
        post_target,
        "Read-only remote reply",
    )));

    let ctx = crate::ui::testing::themed_context();
    ctx.begin_pass(egui::RawInput::default());
    let action = egui::CentralPanel::default()
        .show(&ctx, |ui| {
            let mut toast = ToastState::default();
            super::super::draw_teams_posts_page(ui, &mut state, &mut discussions, true, &mut toast)
        })
        .inner;
    let _ = ctx.end_pass();

    assert!(action.is_none());
    assert!(state.remote.has_selection());
    assert_eq!(state.remote.loaded_post_count(), 1);
    assert_eq!(state.remote_channel_reply_count(), 1);
    assert!(
        state
            .selected_channel_state()
            .is_some_and(|channel| channel.composer_open)
    );
    assert_eq!(
        state
            .selected_channel_state()
            .map(|channel| channel.draft.as_str()),
        Some("Keep my local preview draft.")
    );
    assert!(!state.has_keyboard_modal());
}
