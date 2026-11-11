use super::{
    CalendarAction, CalendarDate, CalendarState, LocalEventMutationError, RemoteCalendarDirectory,
    RemoteCalendarEvent, calendar_event_id,
};
use crate::integrations::ProviderResourceRef;
use crate::testing::with_escape_press;
use crate::ui::toast::ToastState;
use eframe::egui;

fn render_active_meeting(state: &mut CalendarState) {
    let ctx = crate::ui::testing::themed_context();
    let mut toast = ToastState::default();
    ctx.begin_pass(egui::RawInput::default());
    state.draw_active_meeting(&ctx, &mut toast);
    let _ = ctx.end_pass();
}

fn render_calendar_sidebar(state: &mut CalendarState) {
    let ctx = crate::ui::testing::themed_context();
    let mut toast = ToastState::default();
    ctx.begin_pass(egui::RawInput::default());
    egui::CentralPanel::default().show(&ctx, |ui| {
        state.draw_sidebar(ui, &mut toast);
    });
    let _ = ctx.end_pass();
}

fn selected_event(title: &str, day: usize, time: &str) -> super::SelectedEvent {
    super::SelectedEvent {
        key: format!("test-event-{day}-{title}-{time}"),
        title: title.into(),
        day,
        time: time.into(),
    }
}

fn selected_title(state: &CalendarState) -> Option<&str> {
    state
        .selected_event
        .as_ref()
        .map(|event| event.title.as_str())
}

fn selected_day(state: &CalendarState) -> Option<usize> {
    state.selected_event.as_ref().map(|event| event.day)
}

fn selected_time(state: &CalendarState) -> Option<&str> {
    state
        .selected_event
        .as_ref()
        .map(|event| event.time.as_str())
}

fn remote_directory(title: &str) -> (RemoteCalendarDirectory, ProviderResourceRef) {
    let target = ProviderResourceRef::local_preview("remote-calendar-event-test");
    let event = RemoteCalendarEvent::microsoft365(
        target.clone(),
        title.into(),
        "2026-11-03 14:00".into(),
        "2026-11-03 14:30".into(),
        "Design studio".into(),
        "Ada Lovelace".into(),
        true,
        Some("https://teams.microsoft.com/l/meetup-join/test-event".into()),
        false,
    );
    (
        RemoteCalendarDirectory::microsoft365(
            "Microsoft 365 Calendar".into(),
            "Central European Standard Time".into(),
            true,
            vec![event],
        ),
        target,
    )
}

#[test]
fn remote_calendar_directory_never_enters_local_storage() {
    let mut state = CalendarState::default();
    let stored_before = state.stored();
    let (directory, _) = remote_directory("Provider planning");

    state.replace_remote_directory(directory);

    assert!(state.has_remote_directory());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());

    state.clear_remote_directory();

    assert!(!state.has_remote_directory());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn selecting_a_remote_event_closes_local_editing_without_staging_a_command() {
    let mut state = CalendarState::default();
    state.begin_new_meeting();
    state.meeting_title = "Keep only in the local draft until selection".into();
    let (directory, target) = remote_directory("Provider planning");
    state.replace_remote_directory(directory);

    assert!(state.remote_calendar.select_for_test(target));
    state.on_remote_event_selected();

    assert!(state.remote_calendar.has_selection());
    assert!(state.has_keyboard_modal());
    assert!(!state.show_new_meeting);
    assert!(state.meeting_title.is_empty());
    assert!(state.selected_event.is_none());
    assert!(state.take_actions().is_empty());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn selected_remote_meeting_link_is_session_only_and_requires_an_explicit_request() {
    let mut state = CalendarState::default();
    let stored_before = state.stored();
    let (directory, target) = remote_directory("Provider planning");
    state.replace_remote_directory(directory);
    assert!(state.remote_calendar.select_for_test(target));

    assert!(state.request_selected_remote_meeting_join());
    assert_eq!(
        state.take_remote_meeting_join_links(),
        ["https://teams.microsoft.com/l/meetup-join/test-event"]
    );
    assert!(state.take_remote_meeting_join_links().is_empty());
    assert!(state.take_actions().is_empty());
    assert_eq!(state.stored(), stored_before);
    assert!(!state.is_persistence_dirty());
}

#[test]
fn cancelled_remote_event_cannot_stage_a_meeting_join() {
    let target = ProviderResourceRef::local_preview("remote-calendar-cancelled-event-test");
    let event = RemoteCalendarEvent::microsoft365(
        target.clone(),
        "Cancelled sync".into(),
        "2026-11-03 14:00".into(),
        "2026-11-03 14:30".into(),
        "Design studio".into(),
        "Ada Lovelace".into(),
        true,
        Some("https://teams.microsoft.com/l/meetup-join/cancelled".into()),
        true,
    );
    let mut state = CalendarState::default();
    state.replace_remote_directory(RemoteCalendarDirectory::microsoft365(
        "Microsoft 365 Calendar".into(),
        "UTC".into(),
        false,
        vec![event],
    ));
    assert!(state.remote_calendar.select_for_test(target));

    assert!(!state.request_selected_remote_meeting_join());
    assert!(state.take_remote_meeting_join_links().is_empty());
    assert!(state.take_actions().is_empty());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn clearing_the_remote_calendar_drops_an_unopened_meeting_link() {
    let mut state = CalendarState::default();
    let (directory, target) = remote_directory("Provider planning");
    state.replace_remote_directory(directory);
    assert!(state.remote_calendar.select_for_test(target));
    assert!(state.request_selected_remote_meeting_join());

    state.clear_remote_directory();

    assert!(state.take_remote_meeting_join_links().is_empty());
    assert!(state.take_actions().is_empty());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn replacing_remote_calendar_keeps_selection_by_opaque_target() {
    let mut state = CalendarState::default();
    let (directory, target) = remote_directory("Original provider title");
    state.replace_remote_directory(directory);
    assert!(state.remote_calendar.select_for_test(target.clone()));

    let replacement = RemoteCalendarDirectory::microsoft365(
        "Renamed remote calendar".into(),
        "UTC".into(),
        false,
        vec![RemoteCalendarEvent::microsoft365(
            target,
            "Renamed provider title".into(),
            "2026-11-03 15:00".into(),
            "2026-11-03 15:30".into(),
            String::new(),
            String::new(),
            false,
            None,
            false,
        )],
    );

    state.replace_remote_directory(replacement);

    assert!(state.remote_calendar.has_selection());
    assert!(!state.is_persistence_dirty());
}

#[test]
fn escape_closes_calendar_dialogs_without_changing_the_selected_day() {
    let mut state = CalendarState {
        selected_day: 3,
        ..Default::default()
    };
    state.open_calendar_picker();
    let mut toast = ToastState::default();

    with_escape_press(|ctx| state.draw_picker(ctx, &mut toast));
    assert!(!state.picker_open);
    assert_eq!(state.selected_day, 3);

    state.select_event("Design review", 3, "Thursday · 14:00–14:30");
    with_escape_press(|ctx| state.draw_selected_event_window(ctx, &mut toast, false));
    assert!(state.selected_event.is_none());
    assert!(selected_time(&state).is_none());
    assert_eq!(state.selected_day, 3);
}

#[test]
fn opening_a_calendar_picker_dismisses_event_details() {
    let mut state = CalendarState::default();
    state.select_event("Design review", 3, "Thursday · 14:00–14:30");

    state.open_calendar_picker();

    assert!(state.picker_open);
    assert!(state.selected_event.is_none());
    assert!(selected_day(&state).is_none());
    assert!(selected_time(&state).is_none());
    assert_eq!(state.selected_day, 3);
}

#[test]
fn dismissing_contextual_overlays_keeps_active_meetings_and_selected_day() {
    let mut state = CalendarState {
        picker_open: true,
        show_new_meeting: true,
        meeting_title: "Draft planning session".into(),
        selected_day: 3,
        selected_event: Some(selected_event("Design review", 3, "Thursday · 14:00–14:30")),
        active_meeting: Some("Design review".into()),
        ..Default::default()
    };

    state.dismiss_contextual_overlays();

    assert!(!state.picker_open);
    assert!(!state.show_new_meeting);
    assert!(state.meeting_title.is_empty());
    assert!(state.selected_event.is_none());
    assert!(selected_day(&state).is_none());
    assert!(selected_time(&state).is_none());
    assert_eq!(state.selected_day, 3);
    assert_eq!(state.active_meeting.as_deref(), Some("Design review"));
}

#[test]
fn escape_cancels_a_new_meeting_without_changing_its_selected_day() {
    let mut state = CalendarState {
        show_new_meeting: true,
        selected_day: 3,
        meeting_title: "Draft planning session".into(),
        ..Default::default()
    };
    let mut toast = ToastState::default();

    with_escape_press(|ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            state.draw_new_meeting_form(ui, crate::features::calendar::week_days(0), &mut toast);
        });
    });

    assert!(!state.show_new_meeting);
    assert!(state.meeting_title.is_empty());
    assert_eq!(state.selected_day, 3);
}

#[test]
fn opening_a_new_meeting_clears_the_previous_event_detail() {
    let mut state = CalendarState {
        selected_day: 2,
        selected_event: Some(selected_event(
            "Design review",
            2,
            "Wednesday · 14:00–14:30",
        )),
        meeting_title: "Stale draft".into(),
        ..Default::default()
    };

    state.begin_new_meeting();

    assert!(state.show_new_meeting);
    assert!(state.new_meeting_focus_requested);
    assert!(state.selected_event.is_none());
    assert!(selected_time(&state).is_none());
    assert!(state.meeting_title.is_empty());
    assert_eq!(state.meeting_day, 2);
}

#[test]
fn calendar_event_ids_are_scoped_to_stable_occurrences() {
    let first = calendar_event_id("local-1");
    let duplicate_title_and_time = calendar_event_id("local-2");
    let seeded_event = calendar_event_id("stall-rota-planning");

    assert_ne!(first, duplicate_title_and_time);
    assert_ne!(first, seeded_event);
    assert_eq!(first, calendar_event_id("local-1"));
}

#[test]
fn meeting_submit_requires_a_title_and_updates_the_selected_event() {
    let mut state = CalendarState {
        show_new_meeting: true,
        ..Default::default()
    };
    assert!(!state.submit_meeting());
    assert!(state.show_new_meeting);

    state.meeting_title = "Design review".into();
    assert!(state.submit_meeting());
    assert_eq!(selected_title(&state), Some("Design review"));
    assert_eq!(selected_time(&state), Some("Tuesday · 14:00–14:30"));
    assert_eq!(state.local_events.len(), 1);
    assert_eq!(state.local_events[0].id, 1);
    assert!(
        state.local_events[0]
            .date
            .matches(crate::features::calendar::week_days(0)[1])
    );
    assert_eq!(state.local_events[0].hour, 14);
    assert!(state.meeting_title.is_empty());
    assert!(!state.show_new_meeting);
    assert_eq!(
        state.take_actions(),
        vec![CalendarAction::CreateEvent {
            title: "Design review".into(),
            starts_at: "2026-11-03T14:00:00".into(),
            ends_at: "2026-11-03T14:30:00".into(),
            online_meeting: false,
        }]
    );
    assert!(state.take_actions().is_empty());
}

#[test]
fn navigation_keeps_work_weeks_contiguous_and_clears_a_selection() {
    let previous_week = crate::features::calendar::week_days(-1);
    let current_week = crate::features::calendar::week_days(0);
    let next_week = crate::features::calendar::week_days(1);
    assert_eq!((previous_week[0].month, previous_week[0].day), (10, 26));
    assert_eq!((current_week[0].month, current_week[0].day), (11, 2));
    assert_eq!((next_week[4].month, next_week[4].day), (11, 13));
    assert_eq!(
        crate::features::calendar::heading(current_week),
        "2026, November 02–06  (Week 45)"
    );

    let mut state = CalendarState::default();
    state.select_event("Planning", 0, "Monday · 09:00–09:30");
    state.change_week(1);
    assert_eq!(state.week_offset, 1);
    assert!(state.selected_event.is_none());
    assert!(selected_time(&state).is_none());
    state.return_to_current_week();
    assert_eq!(state.week_offset, 0);
}

#[test]
fn calendars_are_deduplicated_before_local_persistence() {
    let mut state = CalendarState::default();

    assert!(state.add_calendar("Holidays"));
    assert!(!state.add_calendar("Holidays"));
    assert_eq!(
        state.additional_calendars,
        [super::AdditionalCalendar {
            name: "Holidays".into(),
            visible: true,
        }]
    );
}

#[test]
fn my_calendars_controls_child_visibility_and_disclosure_state() {
    let mut state = CalendarState::default();
    assert!(state.add_calendar("Holidays"));
    assert!(state.all_calendars_visible());
    assert!(state.calendars_open);

    state.set_all_calendars_visible(false);
    assert!(!state.primary_calendar_visible);
    assert!(
        state
            .additional_calendars
            .iter()
            .all(|calendar| !calendar.visible)
    );
    assert!(!state.all_calendars_visible());

    state.set_all_calendars_visible(true);
    state.toggle_calendars();
    assert!(state.all_calendars_visible());
    assert!(!state.calendars_open);
    render_calendar_sidebar(&mut state);
}

#[test]
fn calendar_sidebar_renders_long_calendar_lists_in_its_scroll_region() {
    let mut state = CalendarState {
        additional_calendars: (0..20)
            .map(|index| super::AdditionalCalendar {
                name: format!("Project calendar {index}"),
                visible: true,
            })
            .collect(),
        ..Default::default()
    };

    render_calendar_sidebar(&mut state);

    assert_eq!(state.additional_calendars.len(), 20);
    assert!(state.calendars_open);
    assert!(state.all_calendars_visible());
}

#[test]
fn hiding_the_primary_calendar_hides_its_events_from_search() {
    let mut state = CalendarState::default();

    assert!(!state.search_results("Stall Rota").is_empty());
    assert!(state.schedule_meeting("Design review", 1, 14));
    assert!(!state.search_results("Design review").is_empty());

    state.primary_calendar_visible = false;

    assert!(state.search_results("Stall Rota").is_empty());
    assert!(state.search_results("Design review").is_empty());
}

#[test]
fn hiding_the_primary_calendar_clears_event_details_and_rejects_stale_search_targets() {
    let mut state = CalendarState {
        week_offset: 2,
        event_filter: super::EventFilter::Local,
        ..Default::default()
    };
    state.select_event("Design review", 3, "Thursday · 14:00–14:30");

    state.set_primary_calendar_visible(false);

    assert!(!state.primary_calendar_visible);
    assert!(state.selected_event.is_none());
    assert!(state.is_persistence_dirty());

    state.select_search_result(
        "stall-rota-planning".into(),
        "Stale title".into(),
        -4,
        0,
        "Monday · 09:00–09:30".into(),
    );

    assert!(state.selected_event.is_none());
    assert_eq!(state.week_offset, 2);
    assert_eq!(state.event_filter, super::EventFilter::Local);
}

#[test]
fn search_selection_rehydrates_local_event_details_from_live_calendar_state() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Design review", 3, 11));
    state.clear_event_selection();
    state.week_offset = 2;
    state.event_filter = super::EventFilter::Reference;

    state.select_search_result(
        "local-1".into(),
        "Stale title".into(),
        -4,
        0,
        "Monday · 09:00–09:30".into(),
    );

    assert_eq!(state.week_offset, 0);
    assert_eq!(state.event_filter, super::EventFilter::All);
    assert_eq!(selected_title(&state), Some("Design review"));
    assert_eq!(selected_day(&state), Some(3));
    assert_eq!(selected_time(&state), Some("Thursday · 11:00–11:30"));
}

#[test]
fn scheduling_rejects_unpersistable_data_without_mutating_calendar_state() {
    let mut state = CalendarState::default();
    let too_long_title = "x".repeat(super::MAX_TITLE_BYTES + 1);

    assert!(!state.schedule_meeting(&too_long_title, 1, 14));
    assert!(state.local_events.is_empty());
    assert_eq!(state.next_local_event_id, 1);
    assert!(!state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());

    let event = super::LocalEvent {
        id: 1,
        title: "Existing event".into(),
        date: super::CalendarDate::new(2026, 11, 3).expect("fixture date must be valid"),
        hour: 14,
        duration_minutes: 30,
    };
    state.local_events = vec![event; super::MAX_EVENTS];
    assert!(!state.schedule_meeting("One too many", 1, 14));
    assert_eq!(state.local_events.len(), super::MAX_EVENTS);
    assert_eq!(state.next_local_event_id, 1);
    assert!(!state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());

    state.local_events.clear();
    state.next_local_event_id = u64::MAX;
    assert!(!state.schedule_meeting("Identifier exhausted", 1, 14));
    assert!(state.local_events.is_empty());
    assert_eq!(state.next_local_event_id, u64::MAX);
    assert!(!state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());
}

#[test]
fn channel_meeting_uses_the_selected_channel_name() {
    let mut state = CalendarState::default();

    assert!(state.start_channel_meeting("Bike Commuters · General", false));

    assert_eq!(
        state.local_events[0].title,
        "Bike Commuters · General meeting"
    );
    assert!(state.selected_event.is_none());
    assert_eq!(
        state.active_meeting.as_deref(),
        Some("Bike Commuters · General meeting")
    );
    assert_eq!(
        state.take_actions(),
        vec![CalendarAction::CreateEvent {
            title: "Bike Commuters · General meeting".into(),
            starts_at: "2026-11-03T14:00:00".into(),
            ends_at: "2026-11-03T14:30:00".into(),
            online_meeting: true,
        }]
    );
    assert_eq!(
        state.end_local_meeting(),
        Some("Bike Commuters · General meeting".into())
    );
}

#[test]
fn instant_meetings_are_session_local_and_render_without_a_native_window() {
    let mut state = CalendarState::default();

    assert!(state.start_instant_meeting(false));
    assert_eq!(state.active_meeting.as_deref(), Some("Instant meeting"));
    assert!(state.local_events.is_empty());
    render_active_meeting(&mut state);
    assert_eq!(state.end_local_meeting(), Some("Instant meeting".into()));
}

#[test]
fn active_meeting_blocks_other_entry_points_without_overwriting_its_title() {
    let mut state = CalendarState::default();

    assert!(state.start_instant_meeting(false));
    assert!(!state.start_instant_meeting(false));
    assert!(!state.start_channel_meeting("Bike Commuters · General", false));
    assert_eq!(state.active_meeting.as_deref(), Some("Instant meeting"));
    assert!(state.local_events.is_empty());

    assert_eq!(state.end_local_meeting(), Some("Instant meeting".into()));
    assert!(state.start_channel_meeting("Bike Commuters · General", false));
    assert_eq!(state.local_events.len(), 1);
}

#[test]
fn active_call_blocks_every_calendar_meeting_entry_point() {
    let mut state = CalendarState::default();

    assert!(!state.start_instant_meeting(true));
    assert!(!state.start_channel_meeting("Bike Commuters · General", true));
    assert!(!state.begin_local_meeting("Design review", true));
    assert!(state.active_meeting.is_none());
    assert!(state.local_events.is_empty());
}

#[test]
fn day_layout_limits_the_grid_to_the_selected_weekday() {
    let mut state = CalendarState {
        layout: super::CalendarLayout::Day,
        selected_day: 3,
        ..Default::default()
    };
    let days = crate::features::calendar::week_days(0);

    assert_eq!(state.visible_days(days), vec![(3, days[3])]);
    assert_eq!(state.heading(days), "Thursday, November 05, 2026");

    state.select_event("Review", 3, "Thursday · 09:00–09:30");
    state.change_week(1);
    assert!(state.selected_event.is_none());
    assert!(selected_day(&state).is_none());
    assert!(selected_time(&state).is_none());
}

#[test]
fn focusing_the_sidebar_today_date_opens_the_current_day() {
    let mut state = CalendarState {
        week_offset: 3,
        layout: super::CalendarLayout::WorkWeek,
        selected_day: 4,
        selected_event: Some(selected_event("Old selection", 4, "Friday · 09:00–09:30")),
        ..Default::default()
    };

    state.focus_current_day();

    assert_eq!(state.week_offset, 0);
    assert_eq!(state.layout, super::CalendarLayout::Day);
    assert_eq!(state.selected_day, 1);
    assert!(state.selected_event.is_none());
    assert!(selected_day(&state).is_none());
}

#[test]
fn focusing_a_sidebar_workday_opens_that_day_in_the_current_week() {
    let mut state = CalendarState {
        week_offset: -2,
        layout: super::CalendarLayout::WorkWeek,
        selected_day: 0,
        ..Default::default()
    };

    state.focus_current_weekday(3);

    assert_eq!(state.week_offset, 0);
    assert_eq!(state.layout, super::CalendarLayout::Day);
    assert_eq!(state.selected_day, 3);
}

#[test]
fn event_selection_keeps_its_day_and_time_until_closed() {
    let mut state = CalendarState::default();

    state.select_event("Design sync", 3, "Thursday · 11:00–11:30");

    assert_eq!(selected_title(&state), Some("Design sync"));
    assert_eq!(selected_day(&state), Some(3));
    assert_eq!(selected_time(&state), Some("Thursday · 11:00–11:30"));
    state.clear_event_selection();
    assert!(state.selected_event.is_none());
}

#[test]
fn calendar_dialogs_claim_keyboard_priority() {
    let mut state = CalendarState::default();

    assert!(!state.has_keyboard_modal());
    state.begin_new_meeting();
    assert!(state.has_keyboard_modal());
    state.dismiss_contextual_overlays();
    assert!(!state.has_keyboard_modal());

    state.open_calendar_picker();
    assert!(state.has_keyboard_modal());
    state.select_event("Design sync", 3, "Thursday · 11:00–11:30");
    assert!(state.has_keyboard_modal());
}

#[test]
fn switching_calendar_contexts_dismisses_the_new_meeting_form() {
    let mut state = CalendarState::default();
    state.begin_new_meeting();
    state.meeting_title = "Keep only while scheduling".into();

    state.open_calendar_picker();

    assert!(state.picker_open);
    assert!(!state.show_new_meeting);
    assert!(!state.new_meeting_focus_requested);
    assert!(state.meeting_title.is_empty());

    state.begin_new_meeting();
    state.meeting_title = "Discard when opening event".into();
    state.select_event("Design sync", 3, "Thursday · 11:00–11:30");

    assert!(!state.show_new_meeting);
    assert!(!state.new_meeting_focus_requested);
    assert!(state.meeting_title.is_empty());
    assert_eq!(selected_title(&state), Some("Design sync"));
}

#[test]
fn event_filter_changes_visible_event_kinds_and_clears_a_selection() {
    let mut state = CalendarState::default();
    assert!(state.event_filter.shows_reference());
    assert!(state.event_filter.shows_local());
    state.select_event("Design sync", 3, "Thursday · 11:00–11:30");

    assert!(state.set_event_filter(super::EventFilter::Local));
    assert!(!state.event_filter.shows_reference());
    assert!(state.event_filter.shows_local());
    assert!(state.selected_event.is_none());
    assert!(!state.set_event_filter(super::EventFilter::Local));

    assert!(state.set_event_filter(super::EventFilter::Reference));
    assert!(state.event_filter.shows_reference());
    assert!(!state.event_filter.shows_local());
}

#[test]
fn locally_scheduled_meetings_are_kept_for_their_week_and_day() {
    let mut state = CalendarState {
        meeting_title: "Design review".into(),
        meeting_day: 3,
        meeting_hour: 11,
        show_new_meeting: true,
        ..Default::default()
    };

    assert!(state.submit_meeting());
    assert_eq!(state.local_events.len(), 1);
    assert_eq!(state.local_events[0].date.preview_week_offset(), 0);
    assert!(
        state.local_events[0]
            .date
            .matches(crate::features::calendar::week_days(0)[3])
    );
    assert_eq!(state.local_events[0].hour, 11);
    assert_eq!(selected_time(&state), Some("Thursday · 11:00–11:30"));

    state.change_week(1);
    assert!(
        state
            .local_events
            .iter()
            .all(|event| event.date.preview_week_offset() != state.week_offset)
    );
    state.return_to_current_week();
    assert!(state.local_events.iter().any(|event| {
        event.date.preview_week_offset() == state.week_offset
            && event
                .date
                .matches(crate::features::calendar::week_days(0)[3])
    }));
}

#[test]
fn stored_calendar_restores_domain_state_without_reopening_transient_ui() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Design review", 3, 11));
    assert!(state.add_calendar("Holidays"));
    state.set_all_calendars_visible(false);
    state.toggle_calendars();
    assert!(state.begin_local_meeting("Design review", false));
    state.begin_new_meeting();
    state.change_week(2);

    let snapshot = state.stored();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].id, 1);
    assert_eq!(snapshot.events[0].year, 2026);
    assert_eq!(snapshot.events[0].month, 11);
    assert_eq!(snapshot.events[0].day, 5);
    assert_eq!(snapshot.events[0].duration_minutes, 30);
    assert!(!snapshot.primary_calendar_visible);
    assert!(!snapshot.calendars_open);
    assert_eq!(snapshot.additional_calendars[0].name, "Holidays");
    assert!(!snapshot.additional_calendars[0].visible);

    let mut restored = CalendarState::from_stored(snapshot);
    assert_eq!(restored.week_offset, 0);
    assert_eq!(restored.layout, super::CalendarLayout::WorkWeek);
    assert!(restored.selected_event.is_none());
    assert!(restored.active_meeting.is_none());
    assert!(!restored.show_new_meeting);
    assert!(!restored.picker_open);
    assert!(restored.take_actions().is_empty());
    assert!(!restored.is_persistence_dirty());
    assert_eq!(restored.local_events.len(), 1);
    assert_eq!(restored.local_events[0].id, 1);
    assert!(!restored.primary_calendar_visible);
    assert!(!restored.calendars_open);
    assert_eq!(
        restored.additional_calendars,
        [super::AdditionalCalendar {
            name: "Holidays".into(),
            visible: false,
        }]
    );

    assert!(restored.schedule_meeting("Roadmap review", 4, 15));
    assert_eq!(restored.local_events[1].id, 2);
    assert!(restored.is_persistence_dirty());
    restored.mark_persisted();
    assert!(!restored.is_persistence_dirty());
}

#[test]
fn local_event_edits_are_atomic_keep_their_id_and_survive_a_restart() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Initial design review", 3, 11));
    let _ = state.take_actions();
    state.mark_persisted();
    let original = state.local_events[0].clone();
    assert!(state.begin_local_event_meeting(Some(original.id), "ignored", false));
    assert!(state.begin_editing_selected_local_event());
    assert!(state.selected_event.is_none());
    assert!(state.has_keyboard_modal());

    let editor = state
        .event_editor
        .as_mut()
        .expect("selected local event must open an editor");
    editor.title = "   ".into();
    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::EmptyTitle)
    );
    assert_eq!(
        state.local_events.as_slice(),
        std::slice::from_ref(&original)
    );
    assert!(!state.is_persistence_dirty());

    let editor = state
        .event_editor
        .as_mut()
        .expect("an invalid edit must retain its draft");
    editor.title = "x".repeat(super::MAX_TITLE_BYTES + 1);
    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::TitleTooLong)
    );
    assert_eq!(
        state.local_events.as_slice(),
        std::slice::from_ref(&original)
    );

    let editor = state
        .event_editor
        .as_mut()
        .expect("an invalid edit must retain its draft");
    editor.title = "Revised design review".into();
    editor.hour = super::MAX_EVENT_HOUR + 1;
    editor.duration_minutes = 30;
    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::InvalidHour)
    );
    assert_eq!(
        state.local_events.as_slice(),
        std::slice::from_ref(&original)
    );

    let editor = state
        .event_editor
        .as_mut()
        .expect("an invalid edit must retain its draft");
    editor.hour = 10;
    editor.duration_minutes = super::MIN_DURATION_MINUTES - 1;
    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::InvalidDuration)
    );
    assert_eq!(
        state.local_events.as_slice(),
        std::slice::from_ref(&original)
    );

    let editor = state
        .event_editor
        .as_mut()
        .expect("an invalid edit must retain its draft");
    editor.duration_minutes = 45;
    assert_eq!(state.save_local_event_editor(), Ok(()));
    assert_eq!(state.local_events.len(), 1);
    assert_eq!(state.local_events[0].id, original.id);
    assert_eq!(state.local_events[0].date, original.date);
    assert_eq!(state.local_events[0].title, "Revised design review");
    assert_eq!(state.local_events[0].hour, 10);
    assert_eq!(state.local_events[0].duration_minutes, 45);
    assert_eq!(state.next_local_event_id, 2);
    assert!(state.selected_event.is_none());
    assert!(state.event_editor.is_none());
    assert!(!state.has_keyboard_modal());
    assert_eq!(
        state.active_meeting.as_deref(),
        Some("Revised design review")
    );
    assert_eq!(state.active_local_event_id, Some(original.id));
    assert!(state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());

    let restored = CalendarState::from_stored(state.stored());
    assert_eq!(restored.next_local_event_id, 2);
    assert_eq!(restored.local_events.len(), 1);
    assert_eq!(restored.local_events[0].id, original.id);
    assert_eq!(restored.local_events[0].title, "Revised design review");
    assert_eq!(restored.local_events[0].hour, 10);
    assert_eq!(restored.local_events[0].duration_minutes, 45);
    assert!(restored.active_meeting.is_none());
    assert!(restored.active_local_event_id.is_none());
}

#[test]
fn local_event_reschedules_keep_their_id_and_focus_the_destination_week() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Initial design review", 3, 11));
    let _ = state.take_actions();
    state.mark_persisted();
    let original = state.local_events[0].clone();
    let destination = super::week_days(1)[4];

    assert!(state.begin_local_event_meeting(Some(original.id), "ignored", false));
    assert!(state.begin_editing_selected_local_event());
    let editor = state
        .event_editor
        .as_mut()
        .expect("selected local event must open an editor");
    editor.title = "Rescheduled design review".into();
    editor.date = CalendarDate::from_calendar_day(destination);
    editor.date_picker_week_offset = 1;
    editor.hour = 23;
    editor.duration_minutes = 59;

    assert_eq!(state.save_local_event_editor(), Ok(()));
    let rescheduled = &state.local_events[0];
    assert_eq!(rescheduled.id, original.id);
    assert!(rescheduled.date.matches(destination));
    assert_eq!(rescheduled.title, "Rescheduled design review");
    assert_eq!(rescheduled.hour, 23);
    assert_eq!(rescheduled.duration_minutes, 59);
    assert_eq!(rescheduled.time_label(), "Friday · 23:00–23:59");
    assert_eq!(state.next_local_event_id, 2);
    assert_eq!(state.week_offset, 1);
    assert_eq!(state.selected_day, 4);
    assert!(state.selected_event.is_none());
    assert!(state.event_editor.is_none());
    assert_eq!(
        state.active_meeting.as_deref(),
        Some("Rescheduled design review")
    );
    assert_eq!(state.active_local_event_id, Some(original.id));
    assert!(state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());

    let snapshot = state.stored();
    assert!(super::snapshot_fits_local_store(&snapshot));
    assert_eq!(snapshot.events[0].id, original.id);
    assert_eq!(snapshot.events[0].year, destination.year);
    assert_eq!(snapshot.events[0].month, destination.month);
    assert_eq!(snapshot.events[0].day, destination.day);
    assert_eq!(snapshot.events[0].duration_minutes, 59);

    let restored = CalendarState::from_stored(snapshot);
    assert_eq!(restored.next_local_event_id, 2);
    assert_eq!(restored.local_events.len(), 1);
    assert_eq!(restored.local_events[0].id, original.id);
    assert!(restored.local_events[0].date.matches(destination));
    assert_eq!(restored.local_events[0].duration_minutes, 59);
    assert!(restored.active_meeting.is_none());
    assert!(restored.active_local_event_id.is_none());
}

#[test]
fn local_event_reschedules_reject_dates_outside_the_bounded_workweek_window() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Design review", 3, 11));
    let _ = state.take_actions();
    state.mark_persisted();
    let original = state.local_events[0].clone();
    assert!(state.begin_editing_selected_local_event());

    let editor = state
        .event_editor
        .as_mut()
        .expect("selected local event must open an editor");
    editor.date = CalendarDate::from_calendar_day(
        super::week_days(super::LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS + 1)[0],
    );

    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::InvalidRescheduleDate)
    );
    assert_eq!(state.local_events, [original]);
    assert_eq!(state.week_offset, 0);
    assert!(state.event_editor.is_some());
    assert!(!state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());
}

#[test]
fn calendar_search_omits_an_unrenderable_in_memory_weekend_event() {
    let mut state = CalendarState::default();
    state.local_events.push(super::LocalEvent {
        id: 1,
        title: "Weekend-only fixture".into(),
        date: CalendarDate::new(2026, 11, 7).expect("fixture date must be valid"),
        hour: 11,
        duration_minutes: 30,
    });

    assert!(state.search_results("weekend-only").is_empty());
}

#[test]
fn local_event_edits_cannot_cross_midnight() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Late review", 3, 11));
    state.mark_persisted();
    let original = state.local_events[0].clone();
    assert!(state.begin_editing_selected_local_event());

    let editor = state
        .event_editor
        .as_mut()
        .expect("local event must open an editor");
    let destination = CalendarDate::from_calendar_day(super::week_days(1)[0]);
    editor.date = destination;
    editor.hour = 23;
    editor.duration_minutes = 60;
    assert_eq!(
        state.save_local_event_editor(),
        Err(LocalEventMutationError::InvalidDuration)
    );
    assert_eq!(state.local_events, [original]);
    assert!(!state.is_persistence_dirty());

    let editor = state
        .event_editor
        .as_mut()
        .expect("invalid edit must retain its draft");
    editor.duration_minutes = 59;
    assert_eq!(state.save_local_event_editor(), Ok(()));
    assert_eq!(state.local_events[0].time_label(), "Monday · 23:00–23:59");
    assert_eq!(state.local_events[0].date, destination);
    assert_eq!(state.local_events[0].duration_minutes, 59);
    assert_eq!(state.week_offset, 1);
    assert_eq!(state.selected_day, 0);
}

#[test]
fn reference_events_cannot_enter_the_local_edit_or_delete_lifecycle() {
    let mut state = CalendarState::default();
    state.select_search_result(
        "stall-rota-planning".into(),
        "Copied reference title".into(),
        0,
        1,
        "Tuesday · 11:30–12:00".into(),
    );

    assert!(state.selected_event.is_some());
    assert!(state.selected_local_event_id().is_none());
    assert!(!state.begin_editing_selected_local_event());
    assert_eq!(
        state.delete_selected_local_event(),
        Err(LocalEventMutationError::MissingLocalEvent)
    );
    assert!(state.event_editor.is_none());
    assert!(state.local_events.is_empty());
    assert!(!state.is_persistence_dirty());
    assert!(state.take_actions().is_empty());
}

#[test]
fn deleting_an_active_local_event_clears_selection_and_stale_search() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Disposable local event", 3, 11));
    let _ = state.take_actions();
    state.mark_persisted();
    assert!(state.begin_local_event_meeting(Some(1), "ignored", false));
    assert_eq!(state.active_local_event_id, Some(1));

    assert_eq!(state.delete_selected_local_event(), Ok(()));
    assert!(state.local_events.is_empty());
    assert!(state.selected_event.is_none());
    assert!(state.event_editor.is_none());
    assert!(state.active_meeting.is_none());
    assert!(state.active_local_event_id.is_none());
    assert!(state.is_persistence_dirty());
    assert!(state.search_results("disposable").is_empty());
    assert!(state.take_actions().is_empty());

    state.select_search_result(
        "local-1".into(),
        "Stale local title".into(),
        -4,
        0,
        "Monday · 09:00–09:30".into(),
    );
    assert!(state.selected_event.is_none());

    let mut restored = CalendarState::from_stored(state.stored());
    assert!(restored.local_events.is_empty());
    assert_eq!(restored.next_local_event_id, 2);
    assert!(restored.schedule_meeting("Replacement event", 3, 11));
    assert_eq!(restored.local_events[0].id, 2);
}

#[test]
fn restored_local_event_search_reuses_its_stable_key() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Design review", 3, 11));
    let mut restored = CalendarState::from_stored(state.stored());

    let results = restored.search_results("design review");
    assert_eq!(results.len(), 1);
    let crate::features::search::workspace::WorkspaceSearchTarget::CalendarEvent {
        event_key,
        title,
        week_offset,
        day,
        time,
    } = &results[0].target
    else {
        panic!("local calendar search must resolve to a calendar event");
    };
    assert_eq!(event_key, "local-1");

    restored.select_search_result(
        event_key.clone(),
        title.clone(),
        *week_offset,
        *day,
        time.clone(),
    );
    assert_eq!(
        restored
            .selected_event
            .as_ref()
            .map(|event| event.key.as_str()),
        Some("local-1")
    );
}

#[test]
fn duplicate_local_events_receive_distinct_stable_ids() {
    let mut state = CalendarState::default();

    assert!(state.schedule_meeting("Design review", 1, 14));
    assert!(state.schedule_meeting("Design review", 1, 14));

    assert_eq!(state.local_events.len(), 2);
    assert_ne!(state.local_events[0].id, state.local_events[1].id);
    assert_ne!(
        calendar_event_id(&format!("local-{}", state.local_events[0].id)),
        calendar_event_id(&format!("local-{}", state.local_events[1].id)),
    );
}

#[test]
fn search_can_open_seeded_and_local_calendar_events() {
    let mut state = CalendarState {
        picker_open: true,
        event_filter: super::EventFilter::Local,
        ..Default::default()
    };
    let seeded = state.search_results("rota planning");
    assert_eq!(seeded.len(), 1);
    state.select_search_result(
        "stall-rota-planning".into(),
        "Stall Rota Planning  Wren Castellanos".into(),
        0,
        1,
        "Tuesday · 11:30–12:00".into(),
    );
    assert_eq!(state.selected_day, 1);
    assert_eq!(state.event_filter, super::EventFilter::All);
    assert!(!state.picker_open);
    assert_eq!(
        selected_title(&state),
        Some("Stall Rota Planning  Wren Castellanos")
    );

    assert!(state.schedule_meeting("Roadmap review", 4, 15));
    let local = state.search_results("roadmap");
    assert_eq!(local.len(), 1);
    assert!(matches!(
        local[0].target,
        crate::features::search::workspace::WorkspaceSearchTarget::CalendarEvent { .. }
    ));
}

#[test]
fn channel_meeting_is_scheduled_today_even_when_another_week_is_shown() {
    let mut state = CalendarState::default();
    state.change_week(3);
    state.selected_day = 4;

    assert!(state.start_channel_meeting("Bike Commuters · General", false));

    assert_eq!(
        state.take_actions(),
        vec![CalendarAction::CreateEvent {
            title: "Bike Commuters · General meeting".into(),
            starts_at: "2026-11-03T14:00:00".into(),
            ends_at: "2026-11-03T14:30:00".into(),
            online_meeting: true,
        }]
    );
}

#[test]
fn day_view_steps_one_weekday_and_skips_the_weekend() {
    let mut state = CalendarState {
        layout: super::CalendarLayout::Day,
        selected_day: 3,
        ..Default::default()
    };
    state.select_event("Review", 3, "Thursday · 09:00–09:30");

    state.step_view(1);
    assert_eq!((state.week_offset, state.selected_day), (0, 4));
    assert!(state.selected_event.is_none());

    state.step_view(1);
    assert_eq!((state.week_offset, state.selected_day), (1, 0));
    state.step_view(-1);
    assert_eq!((state.week_offset, state.selected_day), (0, 4));
    state.step_view(-1);
    assert_eq!((state.week_offset, state.selected_day), (0, 3));
}

#[test]
fn work_week_view_still_steps_by_whole_weeks() {
    let mut state = CalendarState {
        selected_day: 2,
        ..Default::default()
    };
    state.step_view(1);
    assert_eq!((state.week_offset, state.selected_day), (1, 2));
    state.step_view(-1);
    state.step_view(-1);
    assert_eq!(state.week_offset, -1);
}

#[test]
fn the_agenda_lists_the_displayed_weeks_events_in_time_order() {
    let mut state = CalendarState::default();
    assert!(state.schedule_meeting("Early sync", 1, 9));
    assert!(state.schedule_meeting("Late review", 1, 15));
    let days = crate::features::calendar::week_days(0);
    let entries = state.agenda_entries(days);
    let tuesday: Vec<_> = entries
        .iter()
        .filter(|entry| entry.day == 1)
        .map(|entry| entry.title.as_str())
        .collect();
    assert_eq!(
        tuesday,
        ["Early sync", "Stall Rota Planning", "Late review"]
    );
    assert!(entries.windows(2).all(|pair| pair[0].day <= pair[1].day));

    state.week_offset = 5;
    assert!(
        state
            .agenda_entries(crate::features::calendar::week_days(5))
            .is_empty()
    );
    state.week_offset = 0;
    state.primary_calendar_visible = false;
    assert!(state.agenda_entries(days).is_empty());
}

#[test]
fn agenda_rows_are_labelled_buttons_that_select_the_event() {
    use egui_kittest::kittest::Queryable;
    let mut state = CalendarState::default();
    let mut toast = ToastState::default();
    let mut installed = false;
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, state: &mut CalendarState| {
            if !installed {
                crate::ui::theme::apply(ui.ctx());
                installed = true;
                ui.ctx().request_discard("install theme fonts");
                return;
            }
            state.draw_agenda(ui, &mut toast);
        },
        std::mem::take(&mut state),
    );
    harness.run();
    harness
        .get_by_label("Tuesday · 11:30–12:00 · Stall Rota Planning")
        .click();
    harness.run();
    assert_eq!(
        harness
            .state()
            .selected_event
            .as_ref()
            .map(|e| e.key.as_str()),
        Some("stall-rota-planning")
    );
}
