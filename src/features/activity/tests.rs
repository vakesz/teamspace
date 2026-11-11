use super::{ActivityAction, ActivityReactionTarget, ActivityState, StoredActivity};
use crate::features::threads::{ThreadChange, ThreadId, ThreadStore, ThreadTarget};
use eframe::egui;
use std::collections::BTreeMap;

fn render_activity_thread(index: usize) {
    let ctx = crate::ui::testing::themed_context();
    let item = super::activity_items()[index];
    let post_reactions = crate::ui::reactions::ReactionSet::from_seed(item.reactions);
    let source_reply_reactions = item
        .source_replies
        .iter()
        .map(|reply| crate::ui::reactions::ReactionSet::from_seed(reply.reactions))
        .collect::<Vec<_>>();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        let _ = super::draw_activity_thread(ui, item, &post_reactions, &source_reply_reactions);
    });
    let _ = ctx.end_pass();
}

fn render_activity_page(state: &mut ActivityState, threads: &mut ThreadStore) {
    let ctx = crate::ui::testing::themed_context();
    let mut toast = crate::ui::toast::ToastState::default();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        let _ = state.draw_page(ui, &mut toast, threads);
    });
    let _ = ctx.end_pass();
}

#[test]
fn reactions_are_scoped_to_each_activity_post_and_source_reply() {
    let activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let parking = super::activity_items()[0];
    let delivery = super::activity_items()[2];

    assert!(
        activity
            .toggle_reaction(&mut threads, parking, ActivityReactionTarget::Post, "👍")
            .expect("reaction must be accepted")
    );
    assert_eq!(
        activity
            .reactions_for(&threads, parking, ActivityReactionTarget::Post)
            .count_for("👍"),
        Some(8)
    );
    assert_eq!(
        activity
            .reactions_for(&threads, delivery, ActivityReactionTarget::Post)
            .count_for("👍"),
        Some(1)
    );

    assert!(
        activity
            .toggle_reaction(
                &mut threads,
                parking,
                ActivityReactionTarget::SourceReply("gate-question"),
                "❤"
            )
            .expect("reaction must be accepted")
    );
    assert_eq!(
        activity
            .reactions_for(
                &threads,
                parking,
                ActivityReactionTarget::SourceReply("gate-question")
            )
            .count_for("❤"),
        Some(2)
    );
    assert_eq!(
        activity
            .reactions_for(
                &threads,
                parking,
                ActivityReactionTarget::SourceReply("gate-response")
            )
            .count_for("❤"),
        Some(2)
    );
}

#[test]
fn new_discussion_mutations_are_owned_by_the_canonical_thread_store() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    activity.select_item(0);
    activity.reply_draft = "Thanks for clarifying.".into();
    assert!(activity.submit_reply(&mut threads).is_ok());
    render_activity_page(&mut activity, &mut threads);
    let parking = super::activity_items()[0];
    assert!(
        activity
            .toggle_reaction(&mut threads, parking, ActivityReactionTarget::Post, "👍")
            .expect("reaction must be accepted")
    );
    activity.filter = super::ActivityFilter::All;
    activity.reply_draft = "Transient draft".into();

    let snapshot = activity.stored();
    assert_eq!(snapshot.read_item_keys, vec!["parking-changes"]);
    assert!(snapshot.replies_by_item.is_empty());
    assert!(snapshot.selected_reactions_by_target.is_empty());

    let restored = ActivityState::from_stored(snapshot);
    assert_eq!(restored.filter, super::ActivityFilter::Unread);
    assert_eq!(restored.selected_item, 0);
    assert!(restored.reply_draft.is_empty());
    assert!(!restored.is_unread(0));
    assert_eq!(
        threads
            .replies(&super::activity_thread_id(parking))
            .first()
            .map(|reply| reply.body()),
        Some("Thanks for clarifying.")
    );
    assert_eq!(
        restored
            .reactions_for(&threads, parking, ActivityReactionTarget::Post)
            .count_for("👍"),
        Some(8)
    );
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn activity_reply_is_immediately_visible_to_teams_search_on_the_shared_thread() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    activity.reply_draft = "Cross-surface discussion rendezvous".into();
    assert!(activity.submit_reply(&mut threads).is_ok());

    let teams = crate::features::teams::TeamsState::default();
    assert!(teams.search_results(&threads, "rendezvous").iter().any(|result| {
        matches!(
            &result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::TeamPost {
                channel,
                post_key,
            } if channel == "Juniper Row All Hands · Facilities" && post_key == "parking-changes"
        )
    }));
}

#[test]
fn activity_local_reply_edits_and_deletes_update_the_canonical_thread() {
    let item = super::activity_items()[0];
    let thread = super::activity_thread_id(item);
    let reference_reply_body = item.source_replies[0].body;
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let reply = threads
        .append_reply(thread.clone(), "Initial local Activity reply")
        .expect("fixture reply must be accepted");
    let _ = threads.take_changes();

    activity.begin_editing_local_reply(item, &reply);
    *activity
        .editing_local_reply_draft_mut(item, reply.id())
        .expect("the local reply editor must retain its draft") =
        "Revised local Activity reply".into();
    render_activity_page(&mut activity, &mut threads);
    activity
        .save_editing_local_reply(&mut threads, item, reply.id())
        .expect("a valid local reply edit must save");

    assert!(activity.editing_reply.is_none());
    assert_eq!(threads.replies(&thread)[0].id(), reply.id());
    assert_eq!(
        threads.replies(&thread)[0].body(),
        "Revised local Activity reply"
    );
    assert_eq!(item.source_replies[0].body, reference_reply_body);
    assert!(matches!(
        threads.take_changes().as_slice(),
        [ThreadChange::ReplyEdited {
            thread: changed_thread,
            reply: changed_reply,
        }] if changed_thread == &thread
            && changed_reply.id() == reply.id()
            && changed_reply.body() == "Revised local Activity reply"
    ));

    let teams = crate::features::teams::TeamsState::default();
    assert!(teams
        .search_results(&threads, "revised local activity")
        .iter()
        .any(|result| {
            matches!(
                &result.target,
                crate::features::search::workspace::WorkspaceSearchTarget::TeamPost {
                    channel,
                    post_key,
                } if channel == "Juniper Row All Hands · Facilities" && post_key == "parking-changes"
            )
        }));
    assert!(
        activity
            .search_results(&threads, "revised local activity")
            .iter()
            .any(|result| {
                matches!(
                    result.target,
                    crate::features::search::workspace::WorkspaceSearchTarget::Activity(0)
                ) && result.detail.starts_with("Activity reply · You")
            })
    );

    activity
        .delete_local_reply(&mut threads, item, reply.id())
        .expect("the edited local reply must delete");
    assert!(threads.replies(&thread).is_empty());
    assert_eq!(item.source_replies[0].body, reference_reply_body);
    assert!(matches!(
        threads.take_changes().as_slice(),
        [ThreadChange::ReplyDeleted {
            thread: changed_thread,
            reply_id,
        }] if changed_thread == &thread && *reply_id == reply.id()
    ));
    assert!(
        activity
            .search_results(&threads, "revised local activity")
            .is_empty()
    );
    assert!(
        teams
            .search_results(&threads, "revised local activity")
            .is_empty()
    );
}

#[test]
fn changing_activity_selection_or_filter_cancels_an_unsaved_reply_edit() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let first_item = super::activity_items()[0];
    let first_thread = super::activity_thread_id(first_item);
    let first_reply = threads
        .append_reply(first_thread.clone(), "First local reply")
        .expect("fixture reply must be accepted");

    activity.begin_editing_local_reply(first_item, &first_reply);
    *activity
        .editing_local_reply_draft_mut(first_item, first_reply.id())
        .expect("the editor must retain its draft") = "Unsaved first edit".into();
    activity.select_item(1);
    assert!(activity.editing_reply.is_none());
    assert_eq!(
        threads.replies(&first_thread)[0].body(),
        "First local reply"
    );

    let delivery_item = super::activity_items()[2];
    let delivery_thread = super::activity_thread_id(delivery_item);
    let delivery_reply = threads
        .append_reply(delivery_thread.clone(), "Delivery local reply")
        .expect("fixture reply must be accepted");
    activity.select_item(2);
    activity.begin_editing_local_reply(delivery_item, &delivery_reply);
    *activity
        .editing_local_reply_draft_mut(delivery_item, delivery_reply.id())
        .expect("the editor must retain its draft") = "Unsaved delivery edit".into();

    assert!(activity.set_filter(super::ActivityFilter::Mentions));
    assert_eq!(activity.selected_item, 0);
    assert!(activity.editing_reply.is_none());
    assert_eq!(
        threads.replies(&delivery_thread)[0].body(),
        "Delivery local reply"
    );
}

#[test]
fn escape_cancels_an_inline_activity_reply_edit_without_mutating_it() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let item = super::activity_items()[0];
    let thread = super::activity_thread_id(item);
    let reply = threads
        .append_reply(thread.clone(), "Local reply before escape")
        .expect("fixture reply must be accepted");
    activity.begin_editing_local_reply(item, &reply);
    *activity
        .editing_local_reply_draft_mut(item, reply.id())
        .expect("the editor must retain its draft") = "Unsaved inline edit".into();
    let mut toast = crate::ui::toast::ToastState::default();

    crate::testing::with_escape_press(|ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let _ = activity.draw_page(ui, &mut toast, &mut threads);
        });
    });

    assert!(activity.editing_reply.is_none());
    assert_eq!(
        threads.replies(&thread)[0].body(),
        "Local reply before escape"
    );
}

#[test]
fn rendering_discards_an_editor_for_a_reply_removed_by_another_surface() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let item = super::activity_items()[0];
    let thread = super::activity_thread_id(item);
    let reply = threads
        .append_reply(thread.clone(), "Reply to remove from another surface")
        .expect("fixture reply must be accepted");
    activity.begin_editing_local_reply(item, &reply);

    threads
        .delete_reply(thread.clone(), reply.id())
        .expect("fixture reply must delete");
    render_activity_page(&mut activity, &mut threads);

    assert!(threads.replies(&thread).is_empty());
    assert!(activity.editing_reply.is_none());
}

#[test]
fn restoring_ignores_records_for_removed_activity_or_reaction_keys() {
    let stored = StoredActivity {
        schema_version: super::ACTIVITY_SCHEMA_VERSION,
        read_item_keys: vec!["removed-item".into()],
        unread_item_keys: vec!["another-removed-item".into()],
        replies_by_item: BTreeMap::from([(
            "removed-item".into(),
            vec!["No longer relevant".into()],
        )]),
        selected_reactions_by_target: BTreeMap::from([(
            "removed-item:reply:gone".into(),
            vec!["👍".into()],
        )]),
    };

    let restored = ActivityState::from_stored(stored);

    assert!(restored.read_item_keys.is_empty());
    assert!(restored.replies_by_item.is_empty());
    assert!(restored.reaction_overrides.is_empty());
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn legacy_discussion_export_retains_stable_keys_until_the_coordinator_commits_migration() {
    let stored = StoredActivity {
        schema_version: super::ACTIVITY_SCHEMA_VERSION,
        read_item_keys: Vec::new(),
        unread_item_keys: Vec::new(),
        replies_by_item: BTreeMap::from([(
            "parking-changes".into(),
            vec!["Keep this reply during migration.".into()],
        )]),
        selected_reactions_by_target: BTreeMap::from([(
            "parking-changes:reply:gate-question".into(),
            vec!["❤".into()],
        )]),
    };
    let mut activity = ActivityState::from_stored(stored);

    let legacy = activity.legacy_discussion();
    assert_eq!(
        legacy.replies_by_item.get("parking-changes"),
        Some(&vec!["Keep this reply during migration.".into()])
    );
    assert_eq!(
        legacy
            .selected_reactions_by_target
            .get("parking-changes:reply:gate-question"),
        Some(&vec!["❤".into()])
    );
    assert!(!activity.is_persistence_dirty());

    activity.clear_legacy_discussion();
    assert!(activity.legacy_discussion().replies_by_item.is_empty());
    assert!(
        activity
            .legacy_discussion()
            .selected_reactions_by_target
            .is_empty()
    );
    assert!(activity.is_persistence_dirty());
}

#[test]
fn legacy_mapping_keeps_the_shared_parking_thread_and_namespaces_activity_only_items() {
    assert_eq!(
        super::legacy_thread_id_for_item_key("parking-changes"),
        ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
    );
    assert_eq!(
        super::legacy_thread_id_for_item_key("delivery-update"),
        ThreadId::new(
            "Bike Commuters · Winter Gear Swap",
            "activity:delivery-update"
        )
    );
    let (thread, target, seed) =
        super::legacy_reaction_target("parking-changes:reply:gate-question")
            .expect("catalogued legacy reaction target must resolve");
    assert_eq!(
        thread,
        ThreadId::new("Juniper Row All Hands · Facilities", "parking-changes")
            .expect("fixture id is valid")
    );
    assert_eq!(
        target,
        ThreadTarget::source_reply("gate-question").expect("fixture target is valid")
    );
    assert_eq!(seed.first().map(|reaction| reaction.icon), Some("❤"));
    assert!(super::legacy_reaction_target("removed-item:reply:gone").is_none());
}

#[test]
fn reply_requires_content_and_retains_a_nonempty_reply() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    assert!(activity.submit_reply(&mut threads).is_err());

    activity.reply_draft = "Thanks for the update.".into();
    assert!(activity.submit_reply(&mut threads).is_ok());
    assert_eq!(
        threads
            .replies(&super::activity_thread_id(super::activity_items()[0]))
            .first()
            .map(|reply| reply.body()),
        Some("Thanks for the update.")
    );
    assert!(activity.reply_draft.is_empty());
}

#[test]
fn reply_composer_remains_renderable_with_a_long_activity_thread() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let thread = super::activity_thread_id(super::activity_items()[0]);
    for index in 0..24 {
        threads
            .append_reply(thread.clone(), &format!("Local thread reply {index}"))
            .expect("fixture reply must be accepted");
    }
    activity.reply_draft = "Keep this draft available".into();

    render_activity_page(&mut activity, &mut threads);

    assert_eq!(threads.replies(&thread).len(), 24);
    assert_eq!(activity.reply_draft, "Keep this draft available");
}

#[test]
fn selecting_an_activity_changes_the_detail_target_and_clears_a_draft() {
    let mut activity = ActivityState {
        reply_draft: "A reply for the previous activity".into(),
        ..Default::default()
    };

    activity.select_item(2);

    assert_eq!(activity.selected_item, 2);
    assert!(activity.reply_draft.is_empty());
    assert_eq!(
        super::activity_items()[activity.selected_item].channel,
        "Bike Commuters · Winter Gear Swap"
    );
}

#[test]
fn changing_the_filter_selects_a_visible_activity_and_clears_its_stale_draft() {
    let mut activity = ActivityState {
        selected_item: 2,
        reply_draft: "A reply for delivery".into(),
        ..Default::default()
    };

    assert!(activity.set_filter(super::ActivityFilter::Mentions));
    assert!(activity.matches_filter(
        activity.selected_item,
        &super::activity_items()[activity.selected_item]
    ));
    assert!(activity.reply_draft.is_empty());
    assert!(!activity.set_filter(super::ActivityFilter::Mentions));
}

#[test]
fn replies_do_not_leak_between_selected_activities() {
    let mut activity = ActivityState {
        reply_draft: "Reply for the first activity".into(),
        ..Default::default()
    };
    let mut threads = ThreadStore::default();
    assert!(activity.submit_reply(&mut threads).is_ok());

    activity.select_item(1);
    assert!(
        threads
            .replies(&super::activity_thread_id(super::activity_items()[1]))
            .is_empty()
    );
    activity.reply_draft = "Reply for the second activity".into();
    assert!(activity.submit_reply(&mut threads).is_ok());

    activity.select_item(0);
    assert_eq!(
        threads
            .replies(&super::activity_thread_id(super::activity_items()[0]))
            .first()
            .map(|reply| reply.body()),
        Some("Reply for the first activity")
    );
}

#[test]
fn opening_an_unread_activity_marks_it_read_and_can_be_reversed() {
    let mut activity = ActivityState::default();
    assert!(activity.is_unread(0));

    activity.select_item(0);
    assert!(!activity.is_unread(0));
    activity.set_item_unread(0, true);
    assert!(activity.is_unread(0));
}

#[test]
fn initially_read_activity_can_be_marked_unread_and_restored() {
    let mut activity = ActivityState::default();
    assert!(!activity.is_unread(2));

    activity.set_item_unread(2, true);

    assert!(activity.is_unread(2));
    assert_eq!(activity.stored().unread_item_keys, vec!["delivery-update"]);
    let mut restored = ActivityState::from_stored(activity.stored());
    assert!(restored.is_unread(2));
    restored.set_item_unread(2, false);
    assert!(!restored.is_unread(2));
}

#[test]
fn marking_all_activity_read_preserves_local_context_and_sparse_markers() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();
    let selected_item = super::activity_items()[0];
    let thread = super::activity_thread_id(selected_item);
    let reply = threads
        .append_reply(thread.clone(), "Keep this local reply")
        .expect("fixture reply must be accepted");
    activity.begin_editing_local_reply(selected_item, &reply);
    *activity
        .editing_local_reply_draft_mut(selected_item, reply.id())
        .expect("the edit draft must be present") = "Keep this unsaved edit too".into();
    activity.reply_draft = "Keep this composer draft".into();
    assert!(
        activity
            .toggle_reaction(
                &mut threads,
                selected_item,
                ActivityReactionTarget::Post,
                "👍",
            )
            .expect("fixture reaction must be accepted")
    );
    let _ = threads.take_changes();

    // The third source item is read by default, so this creates the
    // inverse (locally unread) marker that bulk-mark must remove rather
    // than replacing marker sets wholesale.
    activity.set_item_unread(2, true);
    activity.mark_persisted();
    assert_eq!(activity.unread_item_count(), 3);

    assert_eq!(activity.mark_all_items_read(), 3);
    assert!((0..super::activity_items().len()).all(|index| !activity.is_unread(index)));
    assert_eq!(activity.selected_item, 0);
    assert_eq!(activity.reply_draft, "Keep this composer draft");
    assert!(activity.is_editing_local_reply(selected_item, reply.id()));
    assert_eq!(
        activity
            .editing_local_reply_draft_mut(selected_item, reply.id())
            .expect("the edit draft must survive bulk read")
            .as_str(),
        "Keep this unsaved edit too"
    );
    assert_eq!(
        threads.replies(&thread).first().map(|reply| reply.body()),
        Some("Keep this local reply")
    );
    assert!(
        activity
            .reactions_for(&threads, selected_item, ActivityReactionTarget::Post)
            .current_user_reacted("👍")
    );
    assert!(threads.take_changes().is_empty());
    assert!(activity.is_persistence_dirty());

    let snapshot = activity.stored();
    assert_eq!(
        snapshot.read_item_keys,
        vec!["parking-changes", "van-booking"]
    );
    assert!(snapshot.unread_item_keys.is_empty());
    let restored = ActivityState::from_stored(snapshot);
    assert!((0..super::activity_items().len()).all(|index| !restored.is_unread(index)));

    activity.mark_persisted();
    assert_eq!(activity.mark_all_items_read(), 0);
    assert!(!activity.is_persistence_dirty());
}

#[test]
fn mention_filters_use_notification_metadata_instead_of_every_activity() {
    let mut activity = ActivityState {
        filter: super::ActivityFilter::Mentions,
        ..Default::default()
    };
    let items = super::activity_items();

    assert!(activity.matches_filter(0, &items[0]));
    assert!(activity.matches_filter(1, &items[1]));
    assert!(!activity.matches_filter(2, &items[2]));

    activity.filter = super::ActivityFilter::TagMentions;
    assert!(activity.matches_filter(0, &items[0]));
    assert!(!activity.matches_filter(1, &items[1]));
}

#[test]
fn captured_activity_replies_remain_separate_from_local_replies() {
    let item = super::activity_items()[0];

    assert_eq!(item.source_replies.len(), 3);
    assert_eq!(item.source_replies[0].author, "Ysolde Varga");
    assert_eq!(item.source_replies[1].author, "Gideon Pruitt");
    assert_eq!(item.source_replies[2].author, "Felix Hartmann");
    render_activity_thread(0);

    let mut activity = ActivityState {
        reply_draft: "Thanks for clarifying.".into(),
        ..Default::default()
    };
    let mut threads = ThreadStore::default();
    assert!(activity.submit_reply(&mut threads).is_ok());
    assert_eq!(item.source_replies.len(), 3);
    assert_eq!(
        threads
            .replies(&super::activity_thread_id(item))
            .first()
            .map(|reply| reply.body()),
        Some("Thanks for clarifying.")
    );
}

#[test]
fn shared_activity_discussion_uses_the_teams_catalog_reference() {
    let item = super::activity_items()[0];
    let post = crate::data::team_catalog::seeded_post_for_channel_key(
        "Juniper Row All Hands · Facilities",
        "parking-changes",
    )
    .expect("the shared parking post must be catalogued");

    assert_eq!(item.name, post.author);
    assert_eq!(item.title, post.title);
    assert_eq!(item.timestamp, post.date);
    assert_eq!(item.body, post.body);
    assert_eq!(item.reactions, post.reactions);
    assert_eq!(item.source_replies.len(), post.source_replies.len());
    assert!(item.source_replies.iter().zip(post.source_replies).all(
        |(activity_reply, team_reply)| {
            activity_reply.key == team_reply.key
                && activity_reply.author == team_reply.author
                && activity_reply.date == team_reply.date
                && activity_reply.body == team_reply.body
                && activity_reply.reactions == team_reply.reactions
        }
    ));
}

#[test]
fn search_finds_an_activity_and_opens_its_thread() {
    let mut activity = ActivityState::default();
    let threads = ThreadStore::default();
    let results = activity.search_results(&threads, "packing slip");

    assert_eq!(results.len(), 1);
    assert!(matches!(
        results[0].target,
        crate::features::search::workspace::WorkspaceSearchTarget::Activity(2)
    ));
    assert!(activity.select_search_result(2));
    assert_eq!(activity.selected_item, 2);
    assert_eq!(activity.filter, super::ActivityFilter::All);
    assert!(activity.matches_filter(2, &super::activity_items()[2]));
}

#[test]
fn activity_navigation_preserves_an_exact_catalogued_post_when_available() {
    let items = super::activity_items();

    assert_eq!(
        items[0].navigation_action(),
        ActivityAction::OpenTeamPost {
            channel: "Juniper Row All Hands · Facilities".into(),
            post_key: "parking-changes".into(),
        }
    );
    assert_eq!(
        items[2].navigation_action(),
        ActivityAction::OpenChannel {
            channel: "Bike Commuters · Winter Gear Swap".into(),
        }
    );
}

#[test]
fn all_activity_remains_available_after_a_search_opened_thread() {
    let mut activity = ActivityState::default();
    let mut toast = crate::ui::toast::ToastState::default();

    assert!(activity.select_search_result(2));
    crate::testing::render_headlessly(|ui| activity.draw_sidebar(ui, &mut toast));

    assert_eq!(activity.filter, super::ActivityFilter::All);
    assert_eq!(
        super::PRIMARY_ACTIVITY_FILTERS,
        [
            super::ActivityFilter::Unread,
            super::ActivityFilter::Mentions,
            super::ActivityFilter::TagMentions,
        ]
    );
}

#[test]
fn search_finds_reference_and_local_thread_reply_content() {
    let mut activity = ActivityState::default();
    let mut threads = ThreadStore::default();

    let reference = activity.search_results(&threads, "visitor parking");
    assert!(reference.iter().any(|result| {
        matches!(
            result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::Activity(0)
        ) && result.detail.starts_with("Activity reply · Felix Hartmann")
    }));

    activity.reply_draft = "The stain comparison is ready for review.".into();
    assert!(activity.submit_reply(&mut threads).is_ok());
    let local = activity.search_results(&threads, "stain comparison");
    assert!(local.iter().any(|result| {
        matches!(
            result.target,
            crate::features::search::workspace::WorkspaceSearchTarget::Activity(0)
        ) && result.detail.starts_with("Activity reply · You")
    }));
}

#[test]
fn reply_submit_errors_keep_their_user_visible_text() {
    use super::ReplySubmitError;
    use crate::features::threads::ThreadError;

    assert_eq!(
        ReplySubmitError::EmptyDraft.to_string(),
        "Write a reply before sending."
    );
    assert_eq!(
        ReplySubmitError::ThreadUnavailable.to_string(),
        "That activity thread is no longer available."
    );
    assert_eq!(
        ReplySubmitError::SaveFailed(ThreadError::EmptyReply).to_string(),
        "Couldn't save this reply locally (a reply needs text)."
    );
}

#[test]
fn the_rail_unread_count_tracks_reading_and_marking_unread() {
    let mut activity = ActivityState::default();
    let initial = activity.unread_count();
    assert!(initial > 0);

    activity.select_item(0);
    assert_eq!(activity.unread_count(), initial - 1);
    activity.set_item_unread(2, true);
    assert_eq!(activity.unread_count(), initial);
}
