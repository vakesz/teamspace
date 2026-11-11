use super::super::TeamsState;
use super::super::persistence::snapshot_fits_local_store;
use super::{render_open_thread, render_selected_channel, render_teams_sidebar};
use crate::app::workspace_coordinator::WorkspaceState as WorkspaceCoordinator;
use crate::features::threads::ThreadStore;
use crate::testing::with_escape_press;
use crate::ui::toast::ToastState;

#[test]
fn escape_closes_the_community_picker_without_joining_a_community() {
    let mut state = TeamsState {
        community_picker_open: true,
        ..Default::default()
    };
    let mut toast = ToastState::default();

    with_escape_press(|ctx| super::super::draw_community_picker(ctx, &mut state, &mut toast));

    assert!(!state.community_picker_open);
    assert!(state.joined_communities.is_empty());
}

#[test]
fn teams_dialogs_claim_keyboard_priority() {
    let mut state = TeamsState::default();

    assert!(!state.has_keyboard_modal());
    state.open_community_picker();
    assert!(state.has_keyboard_modal());
    state.open_link_preview("Cycle Shelter Planning Guide");
    assert!(!state.community_picker_open);
    assert!(state.has_keyboard_modal());

    state.open_community_picker();
    assert!(state.link_preview.is_none());
    assert!(state.community_picker_open);
}

#[test]
fn leaving_teams_dismisses_transient_details_without_discarding_a_channel_draft() {
    let mut state = TeamsState::default();
    state.open_channel_composer();
    let channel = state.selected_channel.clone();
    state
        .channel_state
        .get_mut(&channel)
        .expect("selected channel state")
        .draft = "Keep this local post draft".into();
    assert!(state.open_selected_post_thread("cycle-shelter-quote"));
    state.community_picker_open = true;
    state.open_link_preview("Cycle Shelter Planning Guide");

    state.dismiss_contextual_overlays();

    assert!(state.active_thread.is_none());
    assert!(!state.community_picker_open);
    assert!(state.link_preview.is_none());
    let channel = state
        .channel_state
        .get(&channel)
        .expect("selected channel state");
    assert!(channel.composer_open);
    assert_eq!(channel.draft, "Keep this local post draft");
}

#[test]
fn teams_search_matches_team_names_and_channel_names() {
    let catalog = crate::data::team_catalog::team_catalog();
    let bike_commuters = catalog
        .iter()
        .find(|team| team.name == "Bike Commuters")
        .expect("Bike Commuters is catalogued");
    let all_staff = catalog
        .iter()
        .find(|team| team.name == "Juniper Row All Hands")
        .expect("Juniper Row All Hands is catalogued");
    assert!(super::super::team_matches_filter(
        "bike commuters",
        bike_commuters,
    ));
    assert!(super::super::team_matches_filter("facilities", all_staff,));
    assert!(!super::super::team_matches_filter(
        "unrelated",
        bike_commuters,
    ));
}

#[test]
fn teams_can_collapse_and_reopen_a_group_without_hiding_the_selected_channel() {
    let mut state = TeamsState::default();
    assert!(state.is_team_expanded("Bike Commuters"));

    state.toggle_team_expansion("Bike Commuters");
    assert!(!state.is_team_expanded("Bike Commuters"));

    state.select_team_channel("Bike Commuters · Winter Gear Swap");
    assert!(state.is_team_expanded("Bike Commuters"));
}

#[test]
fn sidebar_sections_collapse_and_reopen_for_relevant_local_actions() {
    let mut state = TeamsState::default();
    assert!(state.teams_and_channels_open);
    assert!(state.communities_open);

    state.toggle_teams_and_channels();
    state.toggle_communities();
    assert!(!state.teams_and_channels_open);
    assert!(!state.communities_open);
    render_teams_sidebar(&mut state);

    state.select_team_channel("Bike Commuters · Winter Gear Swap");
    assert!(state.teams_and_channels_open);

    assert!(!state.communities_open);
    assert!(state.join_community("Sourdough swap"));
    assert!(state.communities_open);
    render_teams_sidebar(&mut state);
}

#[test]
fn teams_sidebar_renders_long_community_lists_in_its_scroll_region() {
    let mut state = TeamsState {
        joined_communities: (0..20).map(|index| format!("Community {index}")).collect(),
        ..Default::default()
    };

    render_teams_sidebar(&mut state);

    assert_eq!(state.joined_communities.len(), 20);
    assert!(state.teams_and_channels_open);
    assert!(state.communities_open);
}

#[test]
fn every_catalogued_channel_renders_in_the_compact_workspace_surface() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();

    for team in crate::data::team_catalog::team_catalog() {
        for channel in team.channels {
            state.select_team_channel(&crate::data::team_catalog::channel_key(team, channel));
            render_selected_channel(&mut state, &mut discussions);
        }
    }
}

#[test]
fn selected_channel_resolves_its_own_team_identity() {
    let team = super::super::team_for_channel("Juniper Row All Hands · Facilities")
        .expect("catalogued team");

    assert_eq!(team.name, "Juniper Row All Hands");
    assert!(super::super::team_for_channel("Unknown team · General").is_none());
}

#[test]
fn see_all_teams_clears_the_active_filter() {
    let mut state = TeamsState {
        team_filter_open: true,
        team_filter_query: "facilities".into(),
        ..Default::default()
    };

    state.show_all_teams();

    assert!(!state.team_filter_open);
    assert!(state.team_filter_query.is_empty());
}

#[test]
fn closing_teams_search_controls_clears_their_hidden_queries() {
    let mut state = TeamsState {
        team_filter_open: true,
        team_filter_query: "facilities".into(),
        team_search_open: true,
        team_search_query: "delivery".into(),
        ..Default::default()
    };

    state.toggle_team_filter();
    assert!(!state.team_filter_open);
    assert!(state.team_filter_query.is_empty());

    state.toggle_channel_search();
    assert!(!state.team_search_open);
    assert!(state.team_search_query.is_empty());
}

#[test]
fn opening_team_filter_persists_the_required_sidebar_expansion() {
    let mut state = TeamsState::default();
    state.toggle_teams_and_channels();
    state.mark_persisted();

    state.toggle_team_filter();

    assert!(state.team_filter_open);
    assert!(state.teams_and_channels_open);
    assert!(state.is_persistence_dirty());
}

#[test]
fn switching_channels_clears_the_previous_channel_search() {
    let mut state = TeamsState {
        team_search_open: true,
        team_search_query: "delivery".into(),
        ..Default::default()
    };

    state.select_team_channel("Juniper Row All Hands · Facilities");

    assert_eq!(state.selected_channel, "Juniper Row All Hands · Facilities");
    assert!(!state.team_search_open);
    assert!(state.team_search_query.is_empty());
}

#[test]
fn opening_a_post_thread_reveals_the_posts_tab_for_the_current_channel() {
    let mut state = TeamsState {
        team_tab: super::super::TeamTab::Notes,
        team_search_open: true,
        team_search_query: "parking".into(),
        ..Default::default()
    };
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    state.team_tab = super::super::TeamTab::Notes;
    state.team_search_open = true;
    state.team_search_query = "parking".into();

    assert!(state.open_selected_post_thread("parking-changes"));

    assert!(state.team_tab == super::super::TeamTab::Posts);
    assert!(!state.team_search_open);
    assert!(state.team_search_query.is_empty());
    assert_eq!(
        state
            .active_thread
            .as_ref()
            .map(|thread| thread.key.as_str()),
        Some("parking-changes")
    );
    render_selected_channel(&mut state, &mut discussions);
}

#[test]
fn opening_activity_channel_requests_the_teams_surface() {
    let mut state = WorkspaceCoordinator::default();

    state.open_team_channel("Juniper Row All Hands · Facilities");
    assert_eq!(
        state.teams().selected_channel,
        "Juniper Row All Hands · Facilities"
    );
    assert_eq!(
        state.take_navigation_request(),
        Some(crate::shared::model::Section::Teams)
    );
    assert_eq!(state.take_navigation_request(), None);
}

#[test]
fn channel_meeting_opens_calendar_with_the_selected_channel_name() {
    let mut state = WorkspaceCoordinator::default();

    assert!(state.start_channel_meeting());

    assert_eq!(
        state.take_navigation_request(),
        Some(crate::shared::model::Section::Calendar)
    );
}

#[test]
fn local_calls_and_meetings_are_mutually_exclusive() {
    let mut call_first = WorkspaceCoordinator::default();
    assert_eq!(
        call_first.start_chat_call("Design review", crate::features::calls::CallMode::Audio),
        Some("Design review".into())
    );
    assert!(!call_first.start_channel_meeting());
    assert_eq!(call_first.take_navigation_request(), None);

    let mut meeting_first = WorkspaceCoordinator::default();
    assert!(meeting_first.start_channel_meeting());
    assert_eq!(
        meeting_first.start_chat_call("Design review", crate::features::calls::CallMode::Video),
        None
    );
}

#[test]
fn shared_image_previews_route_to_the_existing_files_inspector() {
    let mut state = WorkspaceCoordinator::default();

    assert!(state.open_shared_preview("layout-mockup.png"));
    assert_eq!(
        state.take_navigation_request(),
        Some(crate::shared::model::Section::Files)
    );
    assert!(!state.open_shared_preview("Cycle Shelter Planning Guide"));
}

#[test]
fn shared_links_open_local_details_without_navigation() {
    let mut state = TeamsState::default();

    state.open_link_preview("Cycle Shelter Planning Guide");

    assert!(state.link_preview.is_some());
}

#[test]
fn communities_are_profile_local_and_deduplicated() {
    let mut state = TeamsState::default();

    assert!(state.join_community("Sourdough swap"));
    assert!(!state.join_community("Sourdough swap"));
    assert_eq!(state.joined_communities, ["Sourdough swap"]);
}

#[test]
fn removing_a_joined_community_updates_only_the_local_durable_profile() {
    let mut state = TeamsState::default();
    assert!(state.join_community("Sourdough swap"));
    assert!(state.join_community("Craft fairs"));
    state.mark_persisted();

    assert!(state.remove_joined_community("Sourdough swap"));
    assert_eq!(state.joined_communities, ["Craft fairs"]);
    assert!(state.is_persistence_dirty());

    let snapshot = state.stored();
    assert!(snapshot_fits_local_store(&snapshot));
    let restored = TeamsState::from_stored(snapshot);
    assert_eq!(restored.joined_communities, ["Craft fairs"]);
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn removing_a_reference_community_is_a_noop() {
    let mut state = TeamsState::default();
    state.mark_persisted();

    assert!(!state.join_community(super::super::REFERENCE_COMMUNITY_NAME));
    assert!(!state.remove_joined_community(super::super::REFERENCE_COMMUNITY_NAME));
    assert!(state.joined_communities.is_empty());
    assert!(state.stored().joined_communities.is_empty());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn normalized_reference_community_variants_remain_source_owned() {
    let mut state = TeamsState::default();

    assert!(!state.join_community(" whole studio community "));
    assert!(!state.join_community("whole studio community"));
    assert!(!state.remove_joined_community(" Whole Studio Community "));
    assert!(state.joined_communities.is_empty());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn restoring_a_reference_community_variant_discards_and_repairs_it() {
    let stored = super::super::StoredTeams {
        joined_communities: vec![" whole studio community ".into(), "Sourdough swap".into()],
        ..Default::default()
    };

    let restored = TeamsState::from_stored(stored);

    assert_eq!(restored.joined_communities, ["Sourdough swap"]);
    assert!(restored.is_persistence_dirty());
    let repaired = restored.stored();
    assert_eq!(repaired.joined_communities, ["Sourdough swap"]);
    assert!(snapshot_fits_local_store(&repaired));
}

#[test]
fn workspace_search_routes_to_the_owning_surface_and_selection() {
    let mut state = WorkspaceCoordinator::default();
    let team_target = state
        .search_results("facilities")
        .into_iter()
        .find(|result| {
            matches!(
                result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamChannel(_)
            )
        })
        .expect("facilities channel result")
        .target;

    assert_eq!(
        state.select_search_result(team_target),
        crate::shared::model::Section::Teams
    );
    assert_eq!(
        state.teams().selected_channel,
        "Juniper Row All Hands · Facilities"
    );

    let activity_target = state
        .search_results("delivery")
        .into_iter()
        .next()
        .expect("delivery activity result")
        .target;
    assert_eq!(
        state.select_search_result(activity_target),
        crate::shared::model::Section::Activity
    );
}

#[test]
fn workspace_search_opens_reference_and_local_team_posts_in_their_threads() {
    let mut state = WorkspaceCoordinator::default();
    let mut discussions = ThreadStore::default();
    let source_target = state
        .search_results("ashfield")
        .into_iter()
        .find(|result| {
            matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { post_key, .. }
                    if post_key == "cycle-shelter-quote"
            )
        })
        .expect("reference post result")
        .target;

    assert_eq!(
        state.select_search_result(source_target),
        crate::shared::model::Section::Teams
    );
    assert_eq!(state.teams().selected_channel, "Bike Commuters · General");
    assert_eq!(
        state
            .teams()
            .active_thread
            .as_ref()
            .map(|thread| thread.key.as_str()),
        Some("cycle-shelter-quote")
    );
    render_open_thread(state.teams_mut(), &mut discussions);

    state.teams_mut().open_channel_composer();
    state.teams_mut().selected_channel_state_mut().draft = "Quarterly delivery review".into();
    assert!(state.teams_mut().submit_channel_post().is_ok());
    let local_target = state
        .search_results("quarterly")
        .into_iter()
        .find(|result| {
            matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { post_key, .. }
                    if post_key == "local:1"
            )
        })
        .expect("local post result")
        .target;

    assert_eq!(
        state.select_search_result(local_target),
        crate::shared::model::Section::Teams
    );
    assert_eq!(
        state
            .teams()
            .active_thread
            .as_ref()
            .map(|thread| thread.key.as_str()),
        Some("local:1")
    );
    render_open_thread(state.teams_mut(), &mut discussions);
}

#[test]
fn chat_calls_become_active_and_searchable_in_calls_history() {
    let mut state = WorkspaceCoordinator::default();

    assert_eq!(
        state.start_chat_call("Design review", crate::features::calls::CallMode::Video),
        Some("Design review".into())
    );
    assert!(state.search_results("design").iter().any(|result| {
        matches!(
            &result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::CallContact(contact)
                if contact == "Design review"
        )
    }));
}

#[test]
fn local_post_search_matches_the_body_and_labels_results_with_an_excerpt() {
    let mut state = WorkspaceCoordinator::default();
    state.teams_mut().open_channel_composer();
    state.teams_mut().selected_channel_state_mut().draft = "Quarterly delivery review".into();
    assert!(state.teams_mut().submit_channel_post().is_ok());

    // The old fixed label no longer makes every local post match.
    assert!(
        !state
            .search_results("new channel post")
            .iter()
            .any(|result| matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { post_key, .. }
                    if post_key == "local:1"
            ))
    );
    let result = state
        .search_results("quarterly")
        .into_iter()
        .find(|result| {
            matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { post_key, .. }
                    if post_key == "local:1"
            )
        })
        .expect("local post result");
    assert_eq!(result.label, "Quarterly delivery review");
}

#[test]
fn post_excerpt_label_uses_the_first_line_and_truncates() {
    assert_eq!(
        super::super::post_excerpt_label("\n  Hello\nworld"),
        "Hello"
    );
    assert_eq!(super::super::post_excerpt_label("   "), "New channel post");
    let long = "x".repeat(60);
    assert_eq!(
        super::super::post_excerpt_label(&long),
        format!("{}…", "x".repeat(48))
    );
}
