use super::super::persistence::{MAX_POST_BYTES, snapshot_fits_local_store};
use super::super::{LocalPost, TeamsState, TeamsSubmissionError};
use super::{render_open_thread, render_selected_channel};
use crate::features::threads::{ThreadChange, ThreadError, ThreadId, ThreadStore, ThreadTarget};
use crate::testing::with_escape_press;
use crate::ui::avatar::initials_with_fallback;
use crate::ui::toast::ToastState;
use eframe::egui;
use std::collections::BTreeMap;

#[test]
fn sender_initials_use_the_first_two_words() {
    assert_eq!(initials_with_fallback("Wren Castellanos", ""), "WC");
    assert_eq!(initials_with_fallback("You", ""), "Y");
}

#[test]
fn reply_count_link_uses_a_clear_singular_or_plural_label() {
    assert_eq!(super::super::thread_reply_link_label(1), "Open 1 reply");
    assert_eq!(super::super::thread_reply_link_label(12), "Open 12 replies");
}

#[test]
fn channel_post_requires_content_and_clears_the_composer_after_submit() {
    let mut state = TeamsState::default();
    assert!(state.submit_channel_post().is_err());

    state.open_channel_composer();
    assert!(state.has_keyboard_modal());
    assert!(
        state
            .selected_channel_state()
            .is_some_and(|channel| channel.composer_focus_requested)
    );
    state.selected_channel_state_mut().draft = "  Delivery is ready for review.  ".into();
    assert!(state.submit_channel_post().is_ok());
    let channel = state.selected_channel_state().expect("state is retained");
    assert_eq!(
        channel.posts.first().map(|post| post.body.as_str()),
        Some("Delivery is ready for review.")
    );
    assert_eq!(channel.posts.first().map(|post| post.id), Some(1));
    assert!(channel.draft.is_empty());
    assert!(!channel.composer_open);
    assert!(!channel.composer_focus_requested);
    assert!(!state.has_keyboard_modal());
}

#[test]
fn channel_posts_reject_unpersistable_text_before_mutating_local_state() {
    let mut state = TeamsState::default();
    state.open_channel_composer();
    let oversized = "x".repeat(MAX_POST_BYTES + 1);
    state.selected_channel_state_mut().draft = oversized.clone();

    assert_eq!(
        state.submit_channel_post(),
        Err(TeamsSubmissionError::PostTooLong)
    );

    let channel = state
        .selected_channel_state()
        .expect("composer state exists");
    assert!(channel.posts.is_empty());
    assert_eq!(channel.draft, oversized);
    assert!(channel.composer_open);
    assert_eq!(state.next_local_post_id, 1);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn channel_posts_reject_a_prospective_snapshot_that_would_exceed_storage_capacity() {
    let mut state = TeamsState::default();
    state.selected_channel_state_mut().posts = (1..=127)
        .map(|id| LocalPost {
            id,
            body: "x".repeat(MAX_POST_BYTES),
        })
        .collect();
    state.next_local_post_id = 128;
    assert!(snapshot_fits_local_store(&state.stored()));

    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "x".repeat(MAX_POST_BYTES);

    assert_eq!(
        state.submit_channel_post(),
        Err(TeamsSubmissionError::StorageCapacity)
    );

    let channel = state
        .selected_channel_state()
        .expect("composer state exists");
    assert_eq!(channel.posts.len(), 127);
    assert!(channel.composer_open);
    assert_eq!(state.next_local_post_id, 128);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn channel_composer_remains_renderable_with_a_long_feed() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.selected_channel_state_mut().posts = (0..24)
        .map(|index| LocalPost {
            id: index + 1,
            body: format!("Local feed update {index}"),
        })
        .collect();
    state.open_channel_composer();

    render_selected_channel(&mut state, &mut discussions);

    assert!(
        state
            .selected_channel_state()
            .is_some_and(|channel| channel.composer_open)
    );
    state.cancel_channel_composer();
    render_selected_channel(&mut state, &mut discussions);
    assert!(
        state
            .selected_channel_state()
            .is_some_and(|channel| !channel.composer_open)
    );
}

#[test]
fn channel_posts_notes_and_drafts_do_not_leak_between_channels() {
    let mut state = TeamsState::default();
    state.open_channel_composer();
    let channel = state.selected_channel_state_mut();
    channel.draft = "General-only update".into();
    channel.notes = "General-only note".into();
    assert!(state.submit_channel_post().is_ok());

    state.select_team_channel("Juniper Row All Hands · Facilities");
    assert!(state.selected_channel_state().is_none());
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Facilities update".into();
    assert!(state.submit_channel_post().is_ok());

    state.select_team_channel("Bike Commuters · General");
    let general = state
        .selected_channel_state()
        .expect("general channel state");
    assert_eq!(
        general.posts.first().map(|post| post.body.as_str()),
        Some("General-only update")
    );
    assert_eq!(general.notes, "General-only note");
    assert!(!general.composer_open);
}

#[test]
fn durable_teams_state_restores_posts_and_preferences_without_the_canonical_discussion_overlay() {
    let mut state = TeamsState::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Release readiness is confirmed.".into();
    assert!(state.submit_channel_post().is_ok());
    state.selected_channel_state_mut().notes = "Decision: publish after final review.".into();
    state.mark_persistence_dirty();
    state.toggle_teams_and_channels();
    assert!(state.join_community("Sourdough swap"));

    let snapshot = state.stored();
    let mut restored = TeamsState::from_stored(snapshot);
    assert_eq!(
        restored.selected_channel,
        "Juniper Row All Hands · Facilities"
    );
    assert!(!restored.teams_and_channels_open);
    assert_eq!(restored.joined_communities, ["Sourdough swap"]);
    assert_eq!(
        restored
            .selected_channel_state()
            .and_then(|channel| channel.posts.first())
            .map(|post| (post.id, post.body.as_str())),
        Some((1, "Release readiness is confirmed."))
    );
    assert!(restored.legacy_discussion().is_empty());
    assert_eq!(
        restored
            .selected_channel_state()
            .map(|channel| channel.notes.as_str()),
        Some("Decision: publish after final review.")
    );
    assert!(!restored.is_persistence_dirty());

    restored.open_channel_composer();
    restored.selected_channel_state_mut().draft = "A second stable post.".into();
    assert!(restored.submit_channel_post().is_ok());
    assert!(restored.open_selected_post_thread("local:2"));
    assert!(restored.is_persistence_dirty());
}

#[test]
fn restoring_teams_discards_unknown_targets_without_retargeting_local_posts() {
    let stored = super::super::StoredTeams {
        next_local_post_id: 3,
        channels: BTreeMap::from([(
            "Bike Commuters · General".into(),
            super::super::StoredChannelState {
                local_posts: vec![super::super::StoredLocalPost {
                    id: 1,
                    body: "Keep this post.".into(),
                }],
                replies_by_post: BTreeMap::from([
                    ("local:1".into(), vec!["Keep this reply.".into()]),
                    (
                        "unknown-source-post".into(),
                        vec!["Discard this reply.".into()],
                    ),
                ]),
                selected_reactions_by_target: BTreeMap::from([
                    ("local:1".into(), vec!["👍".into()]),
                    ("unknown-source-post".into(), vec!["❤".into()]),
                ]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };

    let restored = TeamsState::from_stored(stored);
    let channel = restored
        .channel_state
        .get("Bike Commuters · General")
        .expect("known channel state is retained");
    assert_eq!(channel.posts.len(), 1);
    assert_eq!(
        channel.legacy_thread_replies.get("local:1"),
        Some(&vec!["Keep this reply.".into()])
    );
    assert!(
        !channel
            .legacy_thread_replies
            .contains_key("unknown-source-post")
    );
    assert!(channel.legacy_reactions.contains_key("local:1"));
    assert!(!channel.legacy_reactions.contains_key("unknown-source-post"));

    let legacy = restored.legacy_discussion();
    assert_eq!(
        legacy.channels["Bike Commuters · General"].replies_by_post["local:1"],
        ["Keep this reply."]
    );
    assert_eq!(
        legacy.channels["Bike Commuters · General"].selected_reactions_by_target["local:1"],
        ["👍"]
    );
}

#[test]
fn clearing_legacy_discussion_preserves_other_teams_data_and_marks_only_teams_dirty() {
    let stored = super::super::StoredTeams {
        next_local_post_id: 2,
        channels: BTreeMap::from([(
            "Bike Commuters · General".into(),
            super::super::StoredChannelState {
                local_posts: vec![super::super::StoredLocalPost {
                    id: 1,
                    body: "Keep this local post.".into(),
                }],
                notes: "Keep this note.".into(),
                replies_by_post: BTreeMap::from([(
                    "local:1".into(),
                    vec!["Import this reply.".into()],
                )]),
                selected_reactions_by_target: BTreeMap::from([(
                    "local:1".into(),
                    vec!["👍".into()],
                )]),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let mut state = TeamsState::from_stored(stored);

    assert!(!state.legacy_discussion().is_empty());
    assert!(!state.is_persistence_dirty());

    state.clear_legacy_discussion();

    assert!(state.legacy_discussion().is_empty());
    assert!(state.is_persistence_dirty());
    let restored = state.stored();
    let channel = &restored.channels["Bike Commuters · General"];
    assert_eq!(channel.local_posts[0].body, "Keep this local post.");
    assert_eq!(channel.notes, "Keep this note.");
    assert!(channel.replies_by_post.is_empty());
    assert!(channel.selected_reactions_by_target.is_empty());
}

#[test]
fn legacy_discussion_resolvers_preserve_stable_post_and_source_reply_targets() {
    let mut state = TeamsState::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    let parking = crate::data::team_catalog::channel_for_key("Juniper Row All Hands · Facilities")
        .expect("facilities is catalogued")
        .posts
        .first()
        .expect("parking post is catalogued");
    let expected_thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("catalogue identifiers are valid");

    assert_eq!(
        state.legacy_thread_id_for_channel_post("Juniper Row All Hands · Facilities", parking.key),
        Some(expected_thread.clone())
    );
    let reply = parking
        .source_replies
        .first()
        .expect("parking source reply is catalogued");
    let source_key = super::super::source_reply_reaction_key(parking.key, reply);
    let (thread, target, seed) = state
        .legacy_reaction_target("Juniper Row All Hands · Facilities", &source_key)
        .expect("catalogued source reply target resolves");
    assert_eq!(thread, expected_thread);
    assert_eq!(
        target,
        ThreadTarget::source_reply(reply.key).expect("catalogue reply key is valid")
    );
    assert_eq!(seed, reply.reactions);

    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "A local post for migration.".into();
    assert!(state.submit_channel_post().is_ok());
    let local_thread = ThreadId::new("Juniper Row All Hands · Facilities", "local:1")
        .expect("generated local identifier is valid");
    assert_eq!(
        state.legacy_thread_id_for_channel_post("Juniper Row All Hands · Facilities", "local:1"),
        Some(local_thread.clone())
    );
    assert!(matches!(
        state.legacy_reaction_target("Juniper Row All Hands · Facilities", "local:1"),
        Some((thread, ThreadTarget::Post, seed)) if thread == local_thread && seed.is_empty()
    ));
    assert!(
        state
            .legacy_reaction_target("Juniper Row All Hands · Facilities", "unknown-target")
            .is_none()
    );
}

#[test]
fn escape_cancels_a_channel_composer_and_clears_its_draft() {
    let mut state = TeamsState::default();
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Unsent status update".into();
    let mut toast = ToastState::default();

    with_escape_press(|ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::super::draw_channel_composer(ui, &mut state, &mut toast);
        });
    });

    let channel = state.selected_channel_state().expect("state is retained");
    assert!(!channel.composer_open);
    assert!(channel.draft.is_empty());
}

#[test]
fn escape_closes_an_inline_thread_and_clears_its_reply_draft() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    assert!(state.open_selected_post_thread("cycle-shelter-quote"));
    state.thread_draft = "Unsent thread reply".into();
    let mut toast = ToastState::default();

    with_escape_press(|ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            super::super::draw_inline_post_thread(ui, &mut state, &mut discussions, &mut toast);
        });
    });

    assert!(state.active_thread.is_none());
    assert!(state.thread_draft.is_empty());
    assert!(!state.has_keyboard_modal());
}

#[test]
fn secondary_channel_tab_notes_are_scoped_to_their_channel_and_tab() {
    let mut state = TeamsState::default();
    state.selected_channel_state_mut().supplier_notes = "Supplier follow-up".into();
    state.selected_channel_state_mut().monthly_notes = "Monthly summary".into();

    state.select_team_channel("Juniper Row All Hands · Facilities");
    let facilities = state.selected_channel_state_mut();
    facilities.supplier_notes = "Facilities supplier note".into();
    assert!(facilities.monthly_notes.is_empty());

    state.select_team_channel("Bike Commuters · General");
    let general = state
        .selected_channel_state()
        .expect("general channel state");
    assert_eq!(general.supplier_notes, "Supplier follow-up");
    assert_eq!(general.monthly_notes, "Monthly summary");
}

#[test]
fn thread_replies_are_scoped_to_their_channel_and_post() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    let locker_access = crate::data::team_catalog::channel_for_key("Bike Commuters · General")
        .expect("Bike Commuters general is catalogued")
        .posts
        .iter()
        .find(|post| post.key == "locker-access")
        .expect("locker access source post is catalogued");
    state.open_post_thread(
        locker_access.key,
        locker_access.title,
        locker_access.source_reply_count,
        locker_access.source_replies,
    );
    assert_eq!(
        state
            .active_thread
            .as_ref()
            .map(|thread| thread.source_replies.len()),
        Some(2)
    );
    render_open_thread(&mut state, &mut discussions);
    state.thread_draft = "I'll check the access today.".into();

    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    assert_eq!(
        state.selected_thread_reply_count(&discussions, "locker-access"),
        1
    );
    assert_eq!(
        state.selected_thread_reply_count(&discussions, "cycle-shelter-quote"),
        0
    );
    assert!(state.thread_draft.is_empty());

    state.select_team_channel("Juniper Row All Hands · Facilities");
    state.open_post_thread("locker-access", "Access to the Bike Locker Room", 0, &[]);
    assert_eq!(
        state.selected_thread_reply_count(&discussions, "locker-access"),
        0
    );
    state.thread_draft = "Facilities-only reply.".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());

    state.select_team_channel("Bike Commuters · General");
    assert_eq!(
        state.selected_thread_reply_count(&discussions, "locker-access"),
        1
    );
}

#[test]
fn thread_replies_emit_canonical_changes_after_successful_local_mutations() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    assert!(state.open_selected_post_thread("cycle-shelter-quote"));

    assert_eq!(
        state.submit_thread_reply(&mut discussions),
        Err(TeamsSubmissionError::Discussion(ThreadError::EmptyReply))
    );
    assert!(discussions.take_changes().is_empty());

    state.thread_draft = "I will review the architecture today.".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    let changes = discussions.take_changes();
    assert_eq!(changes.len(), 1);
    assert!(matches!(
        &changes[0],
        ThreadChange::ReplyAdded { thread, reply }
            if thread.channel() == "Bike Commuters · General"
                && thread.post_key() == "cycle-shelter-quote"
                && reply.body() == "I will review the architecture today."
    ));
    assert!(discussions.take_changes().is_empty());

    // Teams persistence keeps legacy data only for migration; new replies
    // belong exclusively to the canonical discussion projection.
    assert!(state.stored().channels.is_empty());
}

#[test]
fn local_reply_reactions_render_from_the_canonical_overlay() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    assert!(state.open_selected_post_thread("parking-changes"));
    state.thread_draft = "A reply that can receive a local reaction.".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("catalogue identifiers are valid");
    let reply = discussions
        .replies(&thread)
        .first()
        .expect("the submitted local reply must exist");
    let target = ThreadTarget::local_reply(reply.id()).expect("local reply ID must be nonzero");
    let _ = discussions.take_changes();

    assert!(
        state
            .toggle_post_reaction(
                &mut discussions,
                "parking-changes",
                target.clone(),
                &[],
                "❤"
            )
            .expect("local reply reaction must be accepted")
    );
    assert!(
        discussions
            .reactions(&thread, &target, &[])
            .current_user_reacted("❤")
    );
    render_open_thread(&mut state, &mut discussions);
}

#[test]
fn local_reply_edits_and_deletes_are_reflected_in_the_canonical_thread() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    assert!(state.open_selected_post_thread("parking-changes"));
    state.thread_draft = "Initial local reply".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    let thread = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("catalogue identifiers are valid");
    let reply = discussions
        .replies(&thread)
        .first()
        .cloned()
        .expect("the submitted reply must exist");

    let active_thread = state
        .active_thread
        .clone()
        .expect("thread must remain active");
    state.begin_editing_local_reply(&active_thread, &reply);
    *state
        .editing_local_reply_draft_mut(&active_thread, reply.id())
        .expect("editor draft must exist") = "Revised local reply".into();
    state
        .save_editing_local_reply(&mut discussions, &active_thread, reply.id())
        .expect("valid local reply edit must save");
    assert_eq!(discussions.replies(&thread)[0].id(), reply.id());
    assert_eq!(
        discussions.replies(&thread)[0].body(),
        "Revised local reply"
    );

    state
        .delete_local_reply(&mut discussions, &active_thread, reply.id())
        .expect("existing local reply must delete");
    assert!(discussions.replies(&thread).is_empty());
}

#[test]
fn local_post_edits_persist_without_reusing_its_stable_id() {
    let mut state = TeamsState::default();
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Initial local post".into();
    assert!(state.submit_channel_post().is_ok());
    assert!(state.begin_editing_local_post(1));
    *state
        .editing_local_post_draft_mut(1)
        .expect("local post editor must exist") = "Revised local post".into();
    assert!(state.save_editing_local_post(1).is_ok());
    assert_eq!(
        state
            .selected_channel_state()
            .expect("selected channel state must exist")
            .posts[0]
            .body,
        "Revised local post"
    );

    let restored = TeamsState::from_stored(state.stored());
    assert_eq!(
        restored
            .selected_channel_state()
            .expect("restored channel state must exist")
            .posts[0]
            .id,
        1
    );
    assert_eq!(
        restored
            .selected_channel_state()
            .expect("restored channel state must exist")
            .posts[0]
            .body,
        "Revised local post"
    );
}

#[test]
fn deleting_a_local_post_clears_its_canonical_thread_and_search_result() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Disposable local post".into();
    assert!(state.submit_channel_post().is_ok());
    assert!(state.open_selected_post_thread("local:1"));
    state.thread_draft = "A disposable local reply".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    let thread = ThreadId::new("Bike Commuters · General", "local:1")
        .expect("local post identifiers are valid");
    let _ = discussions.take_changes();
    let reply = discussions.replies(&thread)[0].clone();
    discussions
        .toggle_reaction(
            thread.clone(),
            ThreadTarget::local_reply(reply.id()).expect("reply ID must be nonzero"),
            &[],
            "❤",
        )
        .expect("local reaction must be accepted");

    state
        .delete_local_post(&mut discussions, 1)
        .expect("existing local post must delete");

    assert!(
        state
            .selected_channel_state()
            .expect("selected channel state must remain available")
            .posts
            .is_empty()
    );
    assert!(state.active_thread.is_none());
    assert!(discussions.replies(&thread).is_empty());
    assert!(discussions.take_changes().is_empty());
    assert!(!state
        .search_results(&discussions, "disposable")
        .iter()
        .any(|result| {
            matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { post_key, .. }
                    if post_key == "local:1"
            )
        }));
}

#[test]
fn canonical_team_replies_are_searchable_by_their_stable_cross_surface_post_key() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    assert!(state.open_selected_post_thread("parking-changes"));
    state.thread_draft = "Asterion migration rendezvous".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());

    assert_eq!(
        discussions
            .replies(
                &ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
                    .expect("catalogue identifiers are valid"),
            )
            .first()
            .map(|reply| reply.body()),
        Some("Asterion migration rendezvous")
    );
    assert!(state.search_results(&discussions, "asterion").iter().any(|result| {
        matches!(
            &result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::TeamPost { channel, post_key }
                if channel == "Juniper Row All Hands · Facilities" && post_key == "parking-changes"
        ) && result.detail == "Your local reply in Juniper Row All Hands · Facilities"
    }));
}

#[test]
fn only_catalogued_teams_targets_are_transportable_from_the_canonical_store() {
    let parking = ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
        .expect("catalogue identifiers are valid");
    assert!(super::super::is_transportable_thread(&parking));
    assert!(super::super::is_transportable_thread_target(
        &parking,
        &ThreadTarget::source_reply("gate-question").expect("catalogue reply key is valid")
    ));
    assert!(!super::super::is_transportable_thread_target(
        &parking,
        &ThreadTarget::source_reply("activity-only-source-reply")
            .expect("syntactically valid activity-only key")
    ));
    assert!(!super::super::is_transportable_thread_target(
        &parking,
        &ThreadTarget::local_reply(1).expect("positive local reply identifier is valid")
    ));
    assert!(!super::super::is_transportable_thread(
        &ThreadId::new("Juniper Row All Hands · Facilities", "local:1")
            .expect("generated local identifier is valid")
    ));
    assert!(!super::super::is_transportable_thread(
        &ThreadId::new("Unknown Team · General", "parking-changes")
            .expect("syntactically valid unknown identifier")
    ));
}

#[test]
fn thread_replies_reject_unpersistable_text_before_mutating_local_state() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    assert!(state.open_selected_post_thread("cycle-shelter-quote"));
    let oversized = "x".repeat(64 * 1024 + 1);
    state.thread_draft = oversized.clone();

    assert_eq!(
        state.submit_thread_reply(&mut discussions),
        Err(TeamsSubmissionError::Discussion(ThreadError::InvalidReply))
    );

    assert_eq!(
        state.selected_thread_reply_count(&discussions, "cycle-shelter-quote"),
        0
    );
    assert_eq!(state.thread_draft, oversized);
    assert!(!state.is_persistence_dirty());
    assert!(!discussions.is_persistence_dirty());
    assert!(discussions.take_changes().is_empty());
}

#[test]
fn reactions_are_scoped_to_their_channel_post_and_source_reply() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    let parking = crate::data::team_catalog::channel_for_key("Juniper Row All Hands · Facilities")
        .expect("facilities is catalogued")
        .posts
        .first()
        .expect("parking post is catalogued");

    assert!(
        state
            .toggle_post_reaction(
                &mut discussions,
                parking.key,
                ThreadTarget::Post,
                parking.reactions,
                "👍",
            )
            .expect("reaction must be accepted")
    );
    assert_eq!(
        state
            .reactions_for_post(
                &discussions,
                parking.key,
                &ThreadTarget::Post,
                parking.reactions,
            )
            .count_for("👍"),
        Some(8)
    );

    let reply = parking
        .source_replies
        .first()
        .expect("parking reply is catalogued");
    let source_target = ThreadTarget::source_reply(reply.key).expect("catalogue key must be valid");
    assert!(
        state
            .toggle_post_reaction(
                &mut discussions,
                parking.key,
                source_target.clone(),
                reply.reactions,
                "❤",
            )
            .expect("reaction must be accepted")
    );
    assert_eq!(
        state
            .reactions_for_post(&discussions, parking.key, &source_target, reply.reactions,)
            .count_for("❤"),
        Some(2)
    );

    state.select_team_channel("Bike Commuters · General");
    let shelter = crate::data::team_catalog::channel_for_key("Bike Commuters · General")
        .expect("Bike Commuters is catalogued")
        .posts
        .iter()
        .find(|post| post.key == "cycle-shelter-quote")
        .expect("cycle shelter source post is catalogued");
    assert_eq!(
        state
            .reactions_for_post(
                &discussions,
                shelter.key,
                &ThreadTarget::Post,
                shelter.reactions,
            )
            .count_for("👍"),
        Some(1)
    );

    state.select_team_channel("Juniper Row All Hands · Facilities");
    assert_eq!(
        state
            .reactions_for_post(
                &discussions,
                parking.key,
                &ThreadTarget::Post,
                parking.reactions,
            )
            .count_for("👍"),
        Some(8)
    );
}

#[test]
fn reaction_toggles_emit_stable_canonical_changes() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.select_team_channel("Juniper Row All Hands · Facilities");
    let parking = crate::data::team_catalog::channel_for_key("Juniper Row All Hands · Facilities")
        .expect("facilities is catalogued")
        .posts
        .first()
        .expect("parking post is catalogued");

    assert!(
        state
            .toggle_post_reaction(
                &mut discussions,
                parking.key,
                ThreadTarget::Post,
                parking.reactions,
                "👍",
            )
            .expect("reaction must be accepted")
    );
    assert!(matches!(
        discussions.take_changes().as_slice(),
        [ThreadChange::ReactionChanged { thread, target: ThreadTarget::Post, reaction, selected: true }]
            if thread == &ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes").expect("fixture")
                && reaction == "👍"
    ));

    assert!(
        !state
            .toggle_post_reaction(
                &mut discussions,
                parking.key,
                ThreadTarget::Post,
                parking.reactions,
                "👍",
            )
            .expect("reaction must be accepted")
    );
    assert!(matches!(
        discussions.take_changes().as_slice(),
        [ThreadChange::ReactionChanged {
            selected: false,
            ..
        }]
    ));
}

#[test]
fn channel_search_matches_titles_and_bodies_case_insensitively() {
    assert!(super::super::post_matches(
        "locker room",
        "Access to the Bike Locker Room",
        "A team update"
    ));
    assert!(super::super::post_matches(
        "delivery",
        "Update",
        "The delivery is ready"
    ));
    assert!(!super::super::post_matches(
        "unrelated",
        "Update",
        "Delivery ready"
    ));
}

#[test]
fn undoing_a_local_post_delete_restores_the_post_and_its_replies() {
    let mut state = TeamsState::default();
    let mut discussions = ThreadStore::default();
    state.open_channel_composer();
    state.selected_channel_state_mut().draft = "Restorable post".into();
    assert!(state.submit_channel_post().is_ok());
    assert!(state.open_selected_post_thread("local:1"));
    state.thread_draft = "A reply that must come back".into();
    assert!(state.submit_thread_reply(&mut discussions).is_ok());
    let thread = ThreadId::new("Bike Commuters · General", "local:1")
        .expect("local post identifiers are valid");
    state
        .delete_local_post(&mut discussions, 1)
        .expect("existing local post must delete");
    assert!(discussions.replies(&thread).is_empty());
    state.mark_persisted();

    assert!(state.undo_delete_local_post(&mut discussions));
    let posts = &state.selected_channel_state().expect("channel state").posts;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body, "Restorable post");
    assert_eq!(discussions.replies(&thread).len(), 1);
    assert!(state.is_persistence_dirty());
    assert!(!state.undo_delete_local_post(&mut discussions));
}
