mod date;
mod event;
mod forms;
mod grid;
mod names;
mod operations;
mod packing;
mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;
mod remote;

use self::grid::toolbar_text;
use self::operations::{
    LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS, calendar_event_id, local_event_date_label, stored_event,
};

#[cfg(test)]
pub(crate) use self::date::heading;
pub(crate) use self::date::week_days;
use self::event::{CalendarDate, LocalEvent};
use self::persistence::{CALENDAR_SCHEMA_VERSION, StoredAdditionalCalendar};
pub(crate) use self::persistence::{LocalCalendarStore, StoredCalendar};
#[cfg(test)]
use self::persistence::{
    MAX_EVENT_HOUR, MAX_EVENTS, MAX_TITLE_BYTES, MIN_DURATION_MINUTES, snapshot_fits_local_store,
};
pub(crate) use self::remote::{RemoteCalendarDirectory, RemoteCalendarEvent};
use crate::features::search::workspace::{WorkspaceSearchResult, WorkspaceSearchTarget};
use crate::ui::icon::{Icon, IconSize, icon_button};
use crate::ui::theme::{self, font_size, space};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText, Vec2};

pub(crate) struct CalendarState {
    week_offset: i32,
    layout: CalendarLayout,
    event_filter: EventFilter,
    selected_day: usize,
    selected_event: Option<SelectedEvent>,
    event_editor: Option<LocalEventEditor>,
    active_meeting: Option<String>,
    active_local_event_id: Option<u64>,
    show_new_meeting: bool,
    new_meeting_focus_requested: bool,
    meeting_title: String,
    meeting_day: usize,
    meeting_hour: u8,
    local_events: Vec<LocalEvent>,
    next_local_event_id: u64,
    pending_actions: Vec<CalendarAction>,
    /// Explicit, session-only requests to open one integration-normalized
    /// Microsoft 365 meeting link. These are not provider mutation commands,
    /// local calls, or persisted calendar state; the app shell performs the
    /// final browser safety check and launch.
    pending_remote_meeting_join_links: Vec<String>,
    picker_open: bool,
    calendars_open: bool,
    primary_calendar_visible: bool,
    additional_calendars: Vec<AdditionalCalendar>,
    /// A normalized Microsoft 365 projection that never enters the local
    /// preview snapshot or local mutation workflow.
    remote_calendar: remote::RemoteCalendarState,
    persistence_dirty: bool,
}

impl Default for CalendarState {
    fn default() -> Self {
        Self {
            week_offset: 0,
            layout: CalendarLayout::WorkWeek,
            event_filter: EventFilter::All,
            selected_day: 1,
            selected_event: None,
            event_editor: None,
            active_meeting: None,
            active_local_event_id: None,
            show_new_meeting: false,
            new_meeting_focus_requested: false,
            meeting_title: String::new(),
            meeting_day: 1,
            meeting_hour: 14,
            local_events: Vec::new(),
            next_local_event_id: 1,
            pending_actions: Vec::new(),
            pending_remote_meeting_join_links: Vec::new(),
            picker_open: false,
            calendars_open: true,
            primary_calendar_visible: true,
            additional_calendars: Vec::new(),
            remote_calendar: remote::RemoteCalendarState::default(),
            persistence_dirty: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalendarLayout {
    WorkWeek,
    Day,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventFilter {
    All,
    Reference,
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AdditionalCalendar {
    name: String,
    visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedEvent {
    key: String,
    title: String,
    day: usize,
    time: String,
}

/// Temporary presentation state for an edit to one durable local event.
///
/// The stable identifier stays outside this draft. The date is an absolute
/// value, while the adjacent week offset is only a bounded picker viewport.
/// Saving replaces the current local record after validating the full draft.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalEventEditor {
    event_id: u64,
    title: String,
    date: CalendarDate,
    hour: u8,
    duration_minutes: u16,
    date_picker_week_offset: i32,
    original_week_offset: i32,
    focus_requested: bool,
}

impl LocalEventEditor {
    fn from_event(event: &LocalEvent) -> Self {
        let original_week_offset = event.date.preview_week_offset();
        Self {
            event_id: event.id,
            title: event.title.clone(),
            date: event.date,
            hour: event.hour,
            duration_minutes: event.duration_minutes,
            date_picker_week_offset: original_week_offset,
            original_week_offset,
            focus_requested: true,
        }
    }

    fn can_shift_date_picker_week(&self, offset: i32) -> bool {
        let minimum = self
            .original_week_offset
            .saturating_sub(LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS);
        let maximum = self
            .original_week_offset
            .saturating_add(LOCAL_EVENT_RESCHEDULE_WEEK_RADIUS);
        let candidate = self.date_picker_week_offset.saturating_add(offset);
        (minimum..=maximum).contains(&candidate)
    }

    fn shift_date_picker_week(&mut self, offset: i32) {
        if self.can_shift_date_picker_week(offset) {
            self.date_picker_week_offset = self.date_picker_week_offset.saturating_add(offset);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalEventMutationError {
    MissingLocalEvent,
    EmptyTitle,
    TitleTooLong,
    InvalidRescheduleDate,
    InvalidHour,
    InvalidDuration,
    StorageCapacity,
}

impl LocalEventMutationError {
    const fn message(self) -> &'static str {
        match self {
            Self::MissingLocalEvent => "That local event is no longer available.",
            Self::EmptyTitle => "Give the event a title before saving.",
            Self::TitleTooLong => "Event titles are too long to save locally.",
            Self::InvalidRescheduleDate => {
                "Choose a weekday within six months of this local event."
            }
            Self::InvalidHour => "Choose an hour from 00:00 through 23:00.",
            Self::InvalidDuration => "Choose a duration that stays within the event's start date.",
            Self::StorageCapacity => "This change would exceed local calendar storage capacity.",
        }
    }
}

/// A provider-neutral calendar intent accepted by the local calendar.
///
/// Calendar keeps its local event projection as the immediate source of UI
/// feedback. The workspace coordinator drains these transient actions and
/// decides how to stage them with a configured transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CalendarAction {
    CreateEvent {
        title: String,
        /// Local wall-clock ISO-8601 timestamp. A provider adapter is
        /// responsible for applying an account/calendar timezone.
        starts_at: String,
        /// Local wall-clock ISO-8601 timestamp. A provider adapter is
        /// responsible for applying an account/calendar timezone.
        ends_at: String,
        online_meeting: bool,
    },
}

#[derive(Clone, Copy)]
struct SeededEvent {
    id: &'static str,
    title: &'static str,
    day: usize,
    hour: f32,
    hours: f32,
    time: &'static str,
    recurring: bool,
}

impl CalendarLayout {
    const fn label(self) -> &'static str {
        match self {
            Self::WorkWeek => "Work week",
            Self::Day => "Day",
        }
    }
}

impl EventFilter {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "All events",
            Self::Reference => "Reference events",
            Self::Local => "Local events",
        }
    }

    const fn toolbar_label(self) -> &'static str {
        match self {
            Self::All => "Filter",
            Self::Reference => "Reference",
            Self::Local => "Local",
        }
    }

    const fn shows_reference(self) -> bool {
        !matches!(self, Self::Local)
    }

    const fn shows_local(self) -> bool {
        !matches!(self, Self::Reference)
    }
}

impl CalendarState {
    /// Replaces the session-only Microsoft 365 calendar projection. The
    /// normalized directory is deliberately not folded into local events or
    /// storage, so an account change can never write provider data into a
    /// local preview profile.
    pub(crate) fn replace_remote_directory(&mut self, directory: RemoteCalendarDirectory) {
        self.remote_calendar.replace(directory);
    }

    /// Clears only the remote calendar overlay at an account/profile boundary.
    /// Local preview events and preferences remain available after disconnect.
    pub(crate) fn clear_remote_directory(&mut self) {
        self.remote_calendar.clear();
        self.pending_remote_meeting_join_links.clear();
    }

    #[cfg(test)]
    pub(crate) const fn has_remote_directory(&self) -> bool {
        self.remote_calendar.is_initialized()
    }

    /// Rebuilds the durable local calendar projection while intentionally
    /// resetting viewport, picker, selection, and active-meeting UI state.
    pub(crate) fn from_stored(stored: StoredCalendar) -> Self {
        let StoredCalendar {
            next_local_event_id,
            events,
            primary_calendar_visible,
            calendars_open,
            additional_calendars,
            ..
        } = stored;
        let local_events: Vec<LocalEvent> = events
            .into_iter()
            .filter_map(|event| {
                let month = u8::try_from(event.month).ok()?;
                let day = u8::try_from(event.day).ok()?;
                let date = CalendarDate::new(event.year, month, day)?;
                Some(LocalEvent {
                    id: event.id,
                    title: event.title,
                    date,
                    hour: event.hour,
                    duration_minutes: event.duration_minutes,
                })
            })
            .collect();
        let next_after_restored_id = local_events
            .iter()
            .map(|event| event.id)
            .max()
            .map_or(1, |id| id.saturating_add(1));
        let additional_calendars = additional_calendars
            .into_iter()
            .filter_map(|calendar| {
                (!calendar.name.trim().is_empty()).then_some(AdditionalCalendar {
                    name: calendar.name,
                    visible: calendar.visible,
                })
            })
            .collect();
        Self {
            local_events,
            next_local_event_id: next_local_event_id.max(next_after_restored_id).max(1),
            primary_calendar_visible,
            calendars_open,
            additional_calendars,
            ..Self::default()
        }
    }

    /// Captures calendar-owned domain state only. The active local meeting and
    /// all modal, selection, and viewport state deliberately stay transient.
    pub(crate) fn stored(&self) -> StoredCalendar {
        StoredCalendar {
            schema_version: CALENDAR_SCHEMA_VERSION,
            next_local_event_id: self.next_local_event_id,
            events: self.local_events.iter().map(stored_event).collect(),
            primary_calendar_visible: self.primary_calendar_visible,
            calendars_open: self.calendars_open,
            additional_calendars: self
                .additional_calendars
                .iter()
                .map(|calendar| StoredAdditionalCalendar {
                    name: calendar.name.clone(),
                    visible: calendar.visible,
                })
                .collect(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    /// Drains accepted user intents without persisting or replaying them.
    ///
    /// Feature state deliberately exposes calendar domain actions instead of
    /// depending on a provider client or workspace transport directly.
    pub(crate) fn take_actions(&mut self) -> Vec<CalendarAction> {
        std::mem::take(&mut self.pending_actions)
    }

    /// Drains direct user requests to open a selected Microsoft 365 event's
    /// provider-normalized meeting URL. The raw URL never enters persistence,
    /// local-call state, or a workspace transport command.
    pub(crate) fn take_remote_meeting_join_links(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_remote_meeting_join_links)
    }

    fn request_selected_remote_meeting_join(&mut self) -> bool {
        let Some(join_url) = self.remote_calendar.selected_join_url() else {
            return false;
        };
        self.pending_remote_meeting_join_links.push(join_url);
        true
    }

    fn mark_persistence_dirty(&mut self) {
        self.persistence_dirty = true;
    }

    pub(crate) const fn has_keyboard_modal(&self) -> bool {
        self.show_new_meeting
            || self.picker_open
            || self.selected_event.is_some()
            || self.event_editor.is_some()
            || self.remote_calendar.has_selection()
    }

    pub(crate) fn dismiss_contextual_overlays(&mut self) {
        self.picker_open = false;
        self.dismiss_new_meeting_form();
        self.clear_event_selection();
        self.remote_calendar.clear_selection();
    }

    pub(crate) fn search_results(&self, query: &str) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        if !self.primary_calendar_visible {
            return Vec::new();
        }

        let mut results = seeded_events()
            .into_iter()
            .filter(|event| {
                format!("{} {}", event.title, event.time)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|event| WorkspaceSearchResult {
                label: event.title.into(),
                detail: format!("Calendar · {}", event.time),
                target: WorkspaceSearchTarget::CalendarEvent {
                    event_key: event.id.into(),
                    title: event.title.into(),
                    week_offset: 0,
                    day: event.day,
                    time: event.time.into(),
                },
            })
            .collect::<Vec<_>>();
        for event in &self.local_events {
            let week_offset = event.date.preview_week_offset();
            let Some(day) = week_days(week_offset)
                .iter()
                .position(|calendar_day| event.date.matches(*calendar_day))
            else {
                // Defensive only: persisted snapshots reject non-workweek
                // dates, but do not fabricate a Monday target if an invalid
                // in-memory record somehow reaches this projection.
                continue;
            };
            let time = event.time_label();
            if format!("{} {time}", event.title)
                .to_lowercase()
                .contains(&query)
            {
                results.push(WorkspaceSearchResult {
                    label: event.title.clone(),
                    detail: format!("Calendar · {time}"),
                    target: WorkspaceSearchTarget::CalendarEvent {
                        event_key: event.key(),
                        title: event.title.clone(),
                        week_offset,
                        day,
                        time,
                    },
                });
            }
        }
        results
    }

    pub(crate) fn select_search_result(
        &mut self,
        event_key: String,
        _title: String,
        _week_offset: i32,
        _day: usize,
        _time: String,
    ) {
        let resolved_event = self.resolve_search_event(&event_key);
        // Search results are a snapshot of the workspace. Clear an existing
        // contextual overlay even when the target disappeared or was hidden
        // before the user activated that snapshot.
        self.dismiss_contextual_overlays();
        let Some((title, week_offset, day, time)) = resolved_event else {
            return;
        };
        self.week_offset = week_offset;
        self.event_filter = EventFilter::All;
        self.select_event_by_key(&event_key, &title, day, &time);
    }

    pub(crate) fn draw_sidebar(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.add_space(space::XL);
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Calendar")
                    .font(theme::semibold(font_size::TITLE))
                    .color(theme::fg1()),
            );
        });
        egui::ScrollArea::vertical()
            .id_salt("calendar_sidebar_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(space::XL);
                self.draw_month_header(ui);
                self.draw_month_grid(ui, toast);
                ui.add_space(space::SM);
                ui.separator();
                ui.add_space(space::LG);
                self.draw_agenda(ui, toast);
                ui.add_space(space::LG);
                ui.separator();
                ui.add_space(space::LG);
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.add(Icon::Add.image(IconSize::S16));
                    if ui
                        .link(RichText::new("Add calendar").color(theme::brand_fg()))
                        .clicked()
                    {
                        self.open_calendar_picker();
                    }
                });
                ui.add_space(space::XL);
                self.draw_calendar_list(ui, toast);
            });
    }

    pub(crate) fn draw_page(
        &mut self,
        ui: &mut egui::Ui,
        toast: &mut ToastState,
        call_active: bool,
    ) {
        let days = week_days(self.week_offset);
        let remote_event_selected = self.remote_calendar.has_selection();
        let can_start_meeting = !call_active && !self.has_active_meeting();
        let meet_now_enabled = can_start_meeting && !remote_event_selected;
        let meet_now_hint = if remote_event_selected {
            "Open the Microsoft 365 event details to join an available meeting"
        } else if can_start_meeting {
            "Start a local instant meeting"
        } else {
            "Leave the active local meeting before starting another"
        };
        let new_hint = if remote_event_selected {
            "Close the Microsoft 365 event details before creating a local preview event"
        } else {
            "Create a local preview event"
        };
        let flat = |button: egui::Button<'static>| button.frame_when_inactive(false);
        ui.horizontal(|ui| {
            ui.add_space(space::MD);
            ui.menu_button(Icon::PanelLeft.image(IconSize::M20), |ui| {
                self.draw_layout_menu(ui, days);
            })
            .response
            .on_hover_text("Calendar layout");
            if ui
                .add(flat(egui::Button::new((
                    Icon::Calendar.image(IconSize::S16),
                    toolbar_text("Today"),
                ))))
                .clicked()
            {
                self.return_to_current_week();
            }
            let (previous_label, next_label) = match self.layout {
                CalendarLayout::WorkWeek => ("Previous week", "Next week"),
                CalendarLayout::Day => ("Previous day", "Next day"),
            };
            if icon_button(ui, Icon::ChevronLeft, previous_label).clicked() {
                self.step_view(-1);
            }
            if icon_button(ui, Icon::ChevronRight, next_label).clicked() {
                self.step_view(1);
            }
            ui.label(
                RichText::new(self.heading(days))
                    .font(theme::semibold(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.add(Icon::ChevronDown.image(IconSize::S16));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(space::LG);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    let new_menu = egui::containers::menu::MenuButton::from_button(
                        egui::Button::new(
                            Icon::ChevronDown
                                .image(IconSize::S16)
                                .tint(theme::fg_on_brand()),
                        )
                        .fill(theme::brand())
                        .corner_radius(egui::CornerRadius {
                            nw: 0,
                            sw: 0,
                            ne: theme::radius::MD,
                            se: theme::radius::MD,
                        }),
                    )
                    .ui(ui, |ui| {
                        if ui
                            .add_enabled(!remote_event_selected, egui::Button::new("New event"))
                            .on_hover_text(new_hint)
                            .clicked()
                        {
                            self.begin_new_meeting();
                            ui.close();
                        }
                    });
                    new_menu.0.on_hover_text("More ways to create");
                    let new_button = ui.add_enabled(
                        !remote_event_selected,
                        egui::Button::new((
                            Icon::Add.image(IconSize::S16).tint(theme::fg_on_brand()),
                            RichText::new("New")
                                .font(theme::semibold(font_size::BODY))
                                .color(theme::fg_on_brand()),
                        ))
                        .fill(theme::brand())
                        .corner_radius(egui::CornerRadius {
                            nw: theme::radius::MD,
                            sw: theme::radius::MD,
                            ne: 0,
                            se: 0,
                        }),
                    );
                    new_button.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            !remote_event_selected,
                            "New event",
                        )
                    });
                    if new_button.on_hover_text(new_hint).clicked() {
                        self.begin_new_meeting();
                    }
                });
                ui.add_space(space::MD);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 1.0;
                    ui.menu_button(Icon::ChevronDown.image(IconSize::S16), |ui| {
                        if ui
                            .add_enabled(meet_now_enabled, egui::Button::new("Meet now"))
                            .on_hover_text(meet_now_hint)
                            .clicked()
                        {
                            self.meet_now(toast, call_active);
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("Meet now options");
                    if ui
                        .add_enabled(
                            meet_now_enabled,
                            flat(egui::Button::new((
                                Icon::Video.image(IconSize::S16),
                                toolbar_text("Meet now"),
                            ))),
                        )
                        .on_hover_text(meet_now_hint)
                        .clicked()
                    {
                        self.meet_now(toast, call_active);
                    }
                });
                ui.menu_button(Icon::More.image(IconSize::M20), |ui| {
                    if ui.button("Go to today").clicked() {
                        self.return_to_current_week();
                        ui.close();
                    }
                    if ui.button("Add calendar").clicked() {
                        self.open_calendar_picker();
                        ui.close();
                    }
                })
                .response
                .on_hover_text("More options");
                ui.menu_button(
                    (
                        Icon::Filter.image(IconSize::S16),
                        toolbar_text(self.event_filter.toolbar_label()),
                        Icon::ChevronDown.image(IconSize::S16),
                    ),
                    |ui| self.draw_filter_menu(ui, toast),
                );
                ui.menu_button(
                    (
                        toolbar_text(self.layout.label()),
                        Icon::ChevronDown.image(IconSize::S16),
                    ),
                    |ui| self.draw_layout_menu(ui, days),
                );
            });
        });
        if self.show_new_meeting {
            self.draw_new_meeting_form(ui, days, toast);
            ui.add_space(8.0);
        }
        ui.add_space(10.0);
        ui.separator();
        if self.remote_calendar.is_initialized() {
            if remote::draw_agenda(ui, &mut self.remote_calendar) {
                self.on_remote_event_selected();
            }
            ui.add_space(10.0);
        }
        let visible_days = self.visible_days(days);
        self.draw_grid(ui, &visible_days, toast);
        self.draw_selected_event_window(ui.ctx(), toast, call_active);
        if remote::draw_selected_event_window(ui.ctx(), &mut self.remote_calendar) {
            let _ = self.request_selected_remote_meeting_join();
        }
        self.draw_local_event_editor(ui.ctx(), toast);
        // The page owns the picker so the sidebar and page never create two
        // foreground modal layers in one frame.
        self.draw_picker(ui.ctx(), toast);
    }

    pub(crate) fn start_channel_meeting(&mut self, channel: &str, call_active: bool) -> bool {
        if call_active || self.active_meeting.is_some() {
            return false;
        }
        let title = format!("{channel} meeting");
        let event_id = self.next_local_event_id;
        if self.schedule_online_meeting_today(&title, 14) {
            self.clear_event_selection();
            return self.begin_local_event_meeting(Some(event_id), &title, call_active);
        }
        false
    }

    fn meet_now(&mut self, toast: &mut ToastState, call_active: bool) {
        if self.start_instant_meeting(call_active) {
            toast.show("Started a local instant meeting");
        } else {
            toast.show_error("End the active local call or meeting before starting another.");
        }
    }

    fn start_instant_meeting(&mut self, call_active: bool) -> bool {
        self.begin_local_meeting("Instant meeting", call_active)
    }

    pub(crate) const fn has_active_meeting(&self) -> bool {
        self.active_meeting.is_some()
    }

    pub(crate) fn begin_new_meeting(&mut self) {
        self.picker_open = false;
        self.clear_event_selection();
        self.remote_calendar.clear_selection();
        self.meeting_title.clear();
        self.meeting_day = self.selected_day.min(4);
        self.meeting_hour = 14;
        self.show_new_meeting = true;
        self.new_meeting_focus_requested = true;
    }

    /// A selected provider event must not share any local editor, picker, or
    /// local event selection state. This keeps toolbar and modal behavior
    /// honest even though both surfaces appear in the same calendar page.
    fn on_remote_event_selected(&mut self) {
        self.picker_open = false;
        self.dismiss_new_meeting_form();
        self.clear_event_selection();
    }

    fn open_calendar_picker(&mut self) {
        self.dismiss_new_meeting_form();
        self.clear_event_selection();
        self.remote_calendar.clear_selection();
        self.picker_open = true;
    }

    pub(crate) fn draw_active_meeting(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        let Some(title) = self.active_meeting.clone() else {
            return;
        };

        let mut leave_requested = false;
        egui::Window::new("Local meeting")
            .collapsible(false)
            .resizable(false)
            .default_width(390.0)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(38.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 14.0, theme::brand());
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(&title)
                            .font(theme::semibold(theme::font_size::SUBTITLE))
                            .color(theme::fg1()),
                    );
                    ui.label(
                        RichText::new("Connected locally")
                            .font(theme::regular(theme::font_size::SMALL))
                            .color(theme::fg3()),
                    );
                });
                ui.add_space(16.0);
                ui.label(
                    RichText::new(
                        "This local preview does not access cameras, microphones, or the network.",
                    )
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
                );
                ui.add_space(14.0);
                ui.vertical_centered(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Leave meeting").color(theme::fg_on_brand()),
                            )
                            .fill(theme::danger())
                            .corner_radius(7.0),
                        )
                        .clicked()
                    {
                        leave_requested = true;
                    }
                });
            });

        if leave_requested {
            self.end_local_meeting();
            toast.show(format!("Left local meeting: {title}"));
        }
    }
}

fn seeded_events() -> [SeededEvent; 5] {
    [
        SeededEvent {
            id: "stall-rota-planning",
            title: "Stall Rota Planning  Wren Castellanos",
            day: 1,
            hour: 11.5,
            hours: 0.5,
            time: "Tuesday · 11:30–12:00",
            recurring: true,
        },
        SeededEvent {
            id: "open-day-logistics",
            title: "Open day logistics  Room 2B",
            day: 2,
            hour: 15.5,
            hours: 0.5,
            time: "Wednesday · 15:30–16:00",
            recurring: false,
        },
        SeededEvent {
            id: "supplier-catch-up",
            title: "Supplier Catch-up\nDara Whitlock",
            day: 3,
            hour: 15.0,
            hours: 1.0,
            time: "Thursday · 15:00–16:00",
            recurring: true,
        },
        SeededEvent {
            id: "workshop-crew-stand-up",
            title: "Workshop Crew Stand-up",
            day: 3,
            hour: 8.5,
            hours: 0.5,
            time: "Thursday · 08:30–09:00",
            recurring: true,
        },
        SeededEvent {
            id: "quarterly-tidy-up",
            title: "Quarterly Tidy-up\nGideon Pruitt",
            day: 4,
            hour: 14.2,
            hours: 0.75,
            time: "Friday · 14:12–14:57",
            recurring: false,
        },
    ]
}

#[cfg(test)]
mod tests;
