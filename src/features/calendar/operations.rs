use super::date::{PREVIEW_TODAY, week_days, weekday_index};
use super::event::{CalendarDate, LocalEvent};
use super::names::{self as date, Ymd};
use super::persistence::StoredCalendar;
use super::persistence::{
    MAX_EVENT_HOUR, MAX_EVENTS, MAX_TITLE_BYTES, MIN_DURATION_MINUTES, StoredEvent,
    max_duration_for_hour, snapshot_fits_local_store,
};
use super::{
    AdditionalCalendar, CalendarAction, CalendarLayout, CalendarState, EventFilter,
    LocalEventEditor, LocalEventMutationError, SelectedEvent, seeded_events,
};
use eframe::egui::{self};

impl CalendarState {
    pub(super) fn submit_meeting(&mut self) -> bool {
        let title = self.meeting_title.trim().to_owned();
        if title.is_empty() {
            return false;
        }
        if !self.schedule_meeting(&title, self.meeting_day, self.meeting_hour) {
            return false;
        }
        self.meeting_title.clear();
        self.show_new_meeting = false;
        true
    }

    pub(super) fn schedule_meeting(&mut self, title: &str, day: usize, hour: u8) -> bool {
        self.schedule_meeting_with_options(title, day, hour, self.week_offset, false)
    }

    /// Schedules an online meeting today (current week), regardless of which
    /// week the calendar page is currently showing.
    pub(super) fn schedule_online_meeting_today(&mut self, title: &str, hour: u8) -> bool {
        let today = weekday_index(PREVIEW_TODAY.0, PREVIEW_TODAY.1, PREVIEW_TODAY.2) as usize;
        self.schedule_meeting_with_options(title, today.min(4), hour, 0, true)
    }

    fn schedule_meeting_with_options(
        &mut self,
        title: &str,
        day: usize,
        hour: u8,
        week_offset: i32,
        online_meeting: bool,
    ) -> bool {
        let title = title.trim();
        if title.is_empty()
            || title.len() > MAX_TITLE_BYTES
            || self.local_events.len() >= MAX_EVENTS
            || !(9..=15).contains(&hour)
        {
            return false;
        }
        let days = week_days(week_offset);
        let day = day.min(days.len() - 1);
        let id = self.next_local_event_id;
        let Some(next_local_event_id) = self.next_local_event_id.checked_add(1) else {
            return false;
        };
        let event = LocalEvent {
            id,
            title: title.into(),
            date: CalendarDate::from_calendar_day(days[day]),
            hour,
            duration_minutes: 30,
        };
        if !snapshot_fits_local_store(
            &self.prospective_local_event_create_snapshot(&event, next_local_event_id),
        ) {
            return false;
        }
        let event_key = event.key();
        let time = event.time_label();
        let end_minutes = u16::from(event.hour) * 60 + event.duration_minutes;
        let action = CalendarAction::CreateEvent {
            title: event.title.clone(),
            starts_at: local_timestamp(event.date, u16::from(event.hour), 0),
            ends_at: local_timestamp(event.date, end_minutes / 60, end_minutes % 60),
            online_meeting,
        };
        self.next_local_event_id = next_local_event_id;
        self.local_events.push(event);
        self.select_event_by_key(&event_key, title, day, &time);
        self.mark_persistence_dirty();
        self.pending_actions.push(action);
        true
    }

    fn prospective_local_event_create_snapshot(
        &self,
        event: &LocalEvent,
        next_local_event_id: u64,
    ) -> StoredCalendar {
        let mut snapshot = self.stored();
        snapshot.next_local_event_id = next_local_event_id;
        snapshot.events.push(stored_event(event));
        snapshot
    }

    fn prospective_local_event_edit_snapshot(&self, event: &LocalEvent) -> Option<StoredCalendar> {
        let mut snapshot = self.stored();
        let stored_record = snapshot
            .events
            .iter_mut()
            .find(|stored_event| stored_event.id == event.id)?;
        *stored_record = stored_event(event);
        Some(snapshot)
    }

    fn prospective_local_event_delete_snapshot(&self, id: u64) -> Option<StoredCalendar> {
        let mut snapshot = self.stored();
        let before = snapshot.events.len();
        snapshot.events.retain(|event| event.id != id);
        (snapshot.events.len() != before).then_some(snapshot)
    }

    pub(super) fn local_event_id_for_key(&self, event_key: &str) -> Option<u64> {
        self.local_events
            .iter()
            .find(|event| event.key() == event_key)
            .map(|event| event.id)
    }

    pub(super) fn selected_local_event_id(&self) -> Option<u64> {
        self.selected_event
            .as_ref()
            .and_then(|event| self.local_event_id_for_key(&event.key))
    }

    pub(super) fn begin_editing_selected_local_event(&mut self) -> bool {
        let Some(event_id) = self.selected_local_event_id() else {
            return false;
        };
        let Some(event) = self
            .local_events
            .iter()
            .find(|event| event.id == event_id)
            .cloned()
        else {
            return false;
        };
        self.clear_event_selection();
        self.event_editor = Some(LocalEventEditor::from_event(&event));
        true
    }

    pub(super) fn save_local_event_editor(&mut self) -> Result<(), LocalEventMutationError> {
        let editor = self
            .event_editor
            .clone()
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        let title = validated_local_event_title(&editor.title)?.to_owned();
        if editor.hour > MAX_EVENT_HOUR {
            return Err(LocalEventMutationError::InvalidHour);
        }
        if !(MIN_DURATION_MINUTES..=max_duration_for_hour(editor.hour))
            .contains(&editor.duration_minutes)
        {
            return Err(LocalEventMutationError::InvalidDuration);
        }
        let existing = self
            .local_events
            .iter()
            .find(|event| event.id == editor.event_id)
            .cloned()
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        if editor.date != existing.date
            && !is_local_event_reschedule_date(existing.date, editor.date)
        {
            return Err(LocalEventMutationError::InvalidRescheduleDate);
        }
        let event = LocalEvent {
            title,
            date: editor.date,
            hour: editor.hour,
            duration_minutes: editor.duration_minutes,
            ..existing
        };
        let snapshot = self
            .prospective_local_event_edit_snapshot(&event)
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        if !snapshot_fits_local_store(&snapshot) {
            return Err(LocalEventMutationError::StorageCapacity);
        }

        let changed = self
            .local_events
            .iter()
            .find(|current| current.id == event.id)
            .is_some_and(|current| current != &event);
        let current = self
            .local_events
            .iter_mut()
            .find(|current| current.id == event.id)
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        *current = event.clone();
        if self.active_local_event_id == Some(event.id) {
            self.active_meeting = Some(event.title.clone());
        }
        self.clear_event_selection();
        self.focus_local_event_date(event.date);
        if changed {
            self.mark_persistence_dirty();
        }
        Ok(())
    }

    pub(super) fn delete_selected_local_event(&mut self) -> Result<(), LocalEventMutationError> {
        let event_id = self
            .selected_local_event_id()
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        let snapshot = self
            .prospective_local_event_delete_snapshot(event_id)
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        if !snapshot_fits_local_store(&snapshot) {
            return Err(LocalEventMutationError::StorageCapacity);
        }
        let index = self
            .local_events
            .iter()
            .position(|event| event.id == event_id)
            .ok_or(LocalEventMutationError::MissingLocalEvent)?;
        self.local_events.remove(index);
        if self.active_local_event_id == Some(event_id) {
            self.end_local_meeting();
        }
        self.clear_event_selection();
        self.mark_persistence_dirty();
        Ok(())
    }

    pub(super) fn select_event_by_key(&mut self, key: &str, title: &str, day: usize, time: &str) {
        self.picker_open = false;
        self.dismiss_new_meeting_form();
        self.event_editor = None;
        self.remote_calendar.clear_selection();
        self.selected_event = Some(SelectedEvent {
            key: key.into(),
            title: title.into(),
            day,
            time: time.into(),
        });
        self.selected_day = day;
    }

    /// Resolves a global-search target against the current local projection
    /// rather than reopening the copied title, date, and time from an older
    /// search result. This also prevents hidden calendar entries from being
    /// reopened through a stale search overlay.
    pub(super) fn resolve_search_event(
        &self,
        event_key: &str,
    ) -> Option<(String, i32, usize, String)> {
        if !self.primary_calendar_visible {
            return None;
        }

        if let Some(event) = seeded_events()
            .into_iter()
            .find(|event| event.id == event_key)
        {
            return Some((event.title.into(), 0, event.day, event.time.into()));
        }

        let event = self
            .local_events
            .iter()
            .find(|event| event.key() == event_key)?;
        let week_offset = event.date.preview_week_offset();
        let day = week_days(week_offset)
            .iter()
            .position(|calendar_day| event.date.matches(*calendar_day))?;
        Some((event.title.clone(), week_offset, day, event.time_label()))
    }

    #[cfg(test)]
    pub(super) fn select_event(&mut self, title: &str, day: usize, time: &str) {
        let key = format!("test-event-{day}-{title}-{time}");
        self.select_event_by_key(&key, title, day, time);
    }

    pub(super) fn begin_local_meeting(&mut self, title: &str, call_active: bool) -> bool {
        self.begin_local_event_meeting(None, title, call_active)
    }

    pub(super) fn begin_local_event_meeting(
        &mut self,
        local_event_id: Option<u64>,
        title: &str,
        call_active: bool,
    ) -> bool {
        let title = match local_event_id {
            Some(id) => match self.local_events.iter().find(|event| event.id == id) {
                Some(event) => event.title.trim().to_owned(),
                None => return false,
            },
            None => title.trim().to_owned(),
        };
        if title.is_empty() || call_active || self.active_meeting.is_some() {
            return false;
        }
        self.active_meeting = Some(title);
        self.active_local_event_id = local_event_id;
        true
    }

    pub(super) fn end_local_meeting(&mut self) -> Option<String> {
        self.active_local_event_id = None;
        self.active_meeting.take()
    }

    pub(super) fn clear_event_selection(&mut self) {
        self.selected_event = None;
        self.event_editor = None;
    }

    pub(super) fn dismiss_new_meeting_form(&mut self) {
        self.show_new_meeting = false;
        self.new_meeting_focus_requested = false;
        self.meeting_title.clear();
    }

    pub(super) fn set_event_filter(&mut self, filter: EventFilter) -> bool {
        if self.event_filter == filter {
            return false;
        }
        self.event_filter = filter;
        self.clear_event_selection();
        true
    }

    pub(super) fn change_week(&mut self, offset: i32) {
        self.week_offset += offset;
        self.clear_event_selection();
        self.remote_calendar.clear_selection();
    }

    /// Moves the view one step back or forward: a week in the work-week
    /// layout, a single day in the Day layout. The Day layout follows the
    /// Mon-Fri grid, so Next from Friday lands on next Monday and Previous
    /// from Monday on last Friday.
    pub(super) fn step_view(&mut self, direction: i32) {
        match self.layout {
            CalendarLayout::WorkWeek => self.change_week(direction),
            CalendarLayout::Day => {
                let day = self.selected_day.min(4) as i32 + direction;
                if day < 0 {
                    self.change_week(-1);
                    self.selected_day = 4;
                } else if day > 4 {
                    self.change_week(1);
                    self.selected_day = 0;
                } else {
                    self.change_week(0);
                    self.selected_day = day as usize;
                }
            }
        }
    }

    pub(super) fn return_to_current_week(&mut self) {
        self.week_offset = 0;
        self.clear_event_selection();
        self.remote_calendar.clear_selection();
    }

    /// Moves the calendar viewport to a saved local event after an edit so a
    /// reschedule never leaves the user on the event's former workweek.
    fn focus_local_event_date(&mut self, date: CalendarDate) {
        let week_offset = date.preview_week_offset();
        self.week_offset = week_offset;
        if let Some(day) = week_days(week_offset)
            .iter()
            .position(|calendar_day| date.matches(*calendar_day))
        {
            self.selected_day = day;
        }
    }

    pub(super) fn focus_current_day(&mut self) {
        self.focus_current_weekday(1);
    }

    pub(super) fn focus_current_weekday(&mut self, day: usize) {
        self.return_to_current_week();
        self.layout = CalendarLayout::Day;
        self.selected_day = day.min(4);
    }

    pub(super) fn add_calendar(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let normalized_name = name.to_lowercase();
        if self
            .additional_calendars
            .iter()
            .any(|calendar| calendar.name.to_lowercase() == normalized_name)
        {
            return false;
        }
        self.additional_calendars.push(AdditionalCalendar {
            name: name.into(),
            visible: true,
        });
        self.mark_persistence_dirty();
        true
    }

    pub(super) fn all_calendars_visible(&self) -> bool {
        self.primary_calendar_visible
            && self
                .additional_calendars
                .iter()
                .all(|calendar| calendar.visible)
    }

    pub(super) fn set_primary_calendar_visible(&mut self, visible: bool) {
        if self.primary_calendar_visible == visible {
            return;
        }
        self.primary_calendar_visible = visible;
        if !visible {
            // Every currently renderable event belongs to the primary local
            // calendar, so its detail window must not outlive that calendar.
            self.clear_event_selection();
        }
        self.mark_persistence_dirty();
    }

    pub(super) fn set_all_calendars_visible(&mut self, visible: bool) {
        let changed = self.primary_calendar_visible != visible
            || self
                .additional_calendars
                .iter()
                .any(|calendar| calendar.visible != visible);
        self.primary_calendar_visible = visible;
        for calendar in &mut self.additional_calendars {
            calendar.visible = visible;
        }
        if !visible {
            self.clear_event_selection();
        }
        if changed {
            self.mark_persistence_dirty();
        }
    }

    pub(super) fn toggle_calendars(&mut self) {
        self.calendars_open = !self.calendars_open;
        self.mark_persistence_dirty();
    }
}

pub(super) fn calendar_event_id(event_key: &str) -> egui::Id {
    egui::Id::new(("calendar_event", event_key))
}

pub(super) fn stored_event(event: &LocalEvent) -> StoredEvent {
    StoredEvent {
        id: event.id,
        title: event.title.clone(),
        year: event.date.year,
        month: u32::from(event.date.month),
        day: u32::from(event.date.day),
        hour: event.hour,
        duration_minutes: event.duration_minutes,
    }
}

fn validated_local_event_title(title: &str) -> Result<&str, LocalEventMutationError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(LocalEventMutationError::EmptyTitle);
    }
    if title.len() > MAX_TITLE_BYTES {
        return Err(LocalEventMutationError::TitleTooLong);
    }
    Ok(title)
}

/// A local reschedule can move through a useful planning window without
/// allowing a transient editor to navigate an unbounded calendar range. A
/// user can make another local edit later if a longer move is needed.
pub(super) const LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS: i32 = 26;

fn is_local_event_reschedule_date(origin: CalendarDate, candidate: CalendarDate) -> bool {
    let origin_week = origin.preview_week_offset();
    let candidate_week = candidate.preview_week_offset();
    let minimum = origin_week.saturating_sub(LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS);
    let maximum = origin_week.saturating_add(LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS);
    (minimum..=maximum).contains(&candidate_week)
        && week_days(candidate_week)
            .iter()
            .any(|day| candidate.matches(*day))
}

pub(super) fn local_event_date_label(date: CalendarDate) -> String {
    date::long_date(
        Ymd::new(date.year, u32::from(date.month), u32::from(date.day)),
        date.weekday(),
    )
}

/// Formats local preview times without reading a host timezone. Remote
/// adapters receive this unambiguous calendar wall-clock representation and
/// apply an account/calendar timezone if they support remote event creation.
fn local_timestamp(date: CalendarDate, hour: u16, minute: u16) -> String {
    format!(
        "{:04}-{:02}-{:02}T{hour:02}:{minute:02}:00",
        date.year, date.month, date.day
    )
}
