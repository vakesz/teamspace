//! Session-only Microsoft 365 calendar presentation.
//!
//! The local preview calendar deliberately owns its own persistence and
//! mutation workflow. This module keeps normalized provider events separate:
//! callers can render a bounded remote agenda and retain opaque event targets.
//! A provider-normalized HTTPS meeting link may cross one explicit,
//! user-triggered handoff to the app shell, but a Graph event can never become
//! a locally editable record or local meeting by accident.

use crate::integrations::ProviderResourceRef;
use crate::ui::a11y::clickable;
use crate::ui::modal;
use crate::ui::shortcuts::take_escape;
use crate::ui::theme;
use eframe::egui::{self, Color32, RichText, Stroke};

/// A normalized, session-only Microsoft 365 calendar directory.
///
/// `calendar_name`, `time_zone`, and event labels must already be bounded,
/// presentation-safe strings supplied by an integration adapter. `can_edit`
/// preserves the provider capability without enabling a local mutation path.
/// The opaque targets are carried only for stable selection preservation; this
/// feature never exposes a provider identifier or creates one from display
/// text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteCalendarDirectory {
    calendar_name: String,
    time_zone: String,
    can_edit: bool,
    events: Vec<RemoteCalendarEvent>,
}

impl RemoteCalendarDirectory {
    pub(crate) fn microsoft365(
        calendar_name: String,
        time_zone: String,
        can_edit: bool,
        events: Vec<RemoteCalendarEvent>,
    ) -> Self {
        Self {
            calendar_name,
            time_zone,
            can_edit,
            events,
        }
    }
}

/// One presentation-safe provider event in the bounded remote agenda.
///
/// The event intentionally exposes no edit/delete/attendee controls. Those
/// operations need separate Graph permissions and delivery reconciliation. A
/// normalized HTTPS meeting link is retained only to support an explicit
/// system-browser handoff; it is never rendered as text or persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteCalendarEvent {
    target: ProviderResourceRef,
    title: String,
    starts_at: String,
    ends_at: String,
    location: String,
    organizer: String,
    meeting_available: bool,
    join_url: Option<String>,
    cancelled: bool,
}

impl RemoteCalendarEvent {
    // This is a narrow adapter-facing constructor rather than a user-input
    // API: each presentation-safe field is supplied independently by the
    // integration-to-feature conversion, so avoid a mutable builder that
    // could accidentally mix local and remote event state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        title: String,
        starts_at: String,
        ends_at: String,
        location: String,
        organizer: String,
        meeting_available: bool,
        join_url: Option<String>,
        cancelled: bool,
    ) -> Self {
        Self {
            target,
            title,
            starts_at,
            ends_at,
            location,
            organizer,
            meeting_available,
            join_url,
            cancelled,
        }
    }
}

/// Transient remote calendar data and opaque selection state.
#[derive(Default)]
pub(super) struct RemoteCalendarState {
    calendar_name: String,
    time_zone: String,
    can_edit: bool,
    events: Vec<RemoteCalendarEvent>,
    selected_target: Option<ProviderResourceRef>,
    initialized: bool,
}

impl RemoteCalendarState {
    pub(super) fn replace(&mut self, directory: RemoteCalendarDirectory) {
        let selected_target = self.selected_target.take();
        self.calendar_name = directory.calendar_name;
        self.time_zone = directory.time_zone;
        self.can_edit = directory.can_edit;
        self.events = directory.events;
        self.initialized = true;
        self.selected_target = selected_target
            .filter(|target| self.events.iter().any(|event| event.target == *target));
    }

    pub(super) fn clear(&mut self) {
        self.calendar_name.clear();
        self.time_zone.clear();
        self.can_edit = false;
        self.events.clear();
        self.selected_target = None;
        self.initialized = false;
    }

    pub(super) const fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub(super) const fn has_selection(&self) -> bool {
        self.selected_target.is_some()
    }

    pub(super) fn clear_selection(&mut self) {
        self.selected_target = None;
    }

    fn select(&mut self, target: ProviderResourceRef) -> bool {
        if self.events.iter().any(|event| event.target == target) {
            self.selected_target = Some(target);
            true
        } else {
            false
        }
    }

    fn selected(&self) -> Option<&RemoteCalendarEvent> {
        let target = self.selected_target.as_ref()?;
        self.events.iter().find(|event| event.target == *target)
    }

    /// Returns an already-normalized meeting link only for the currently
    /// selected, non-cancelled remote event. The feature caller still has to
    /// receive an explicit user click before it may use this destination.
    pub(super) fn selected_join_url(&self) -> Option<String> {
        let event = self.selected()?;
        (!event.cancelled).then(|| event.join_url.clone()).flatten()
    }

    #[cfg(test)]
    pub(super) fn select_for_test(&mut self, target: ProviderResourceRef) -> bool {
        self.select(target)
    }
}

/// Renders a bounded remote agenda and returns whether an event was selected
/// during this frame. The feature owner uses that signal to dismiss all local
/// event editors before a remote detail view becomes active.
pub(super) fn draw_agenda(ui: &mut egui::Ui, state: &mut RemoteCalendarState) -> bool {
    if !state.is_initialized() {
        return false;
    }

    let events = state.events.clone();
    let calendar_name = if state.calendar_name.is_empty() {
        "Microsoft 365 calendar"
    } else {
        state.calendar_name.as_str()
    };
    let time_zone = state.time_zone.clone();
    let mut selected = None;

    egui::Frame::new()
        .fill(theme::brand_tint_subtle())
        .stroke(Stroke::new(1.0_f32, theme::brand_stroke_subtle()))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Microsoft 365 agenda")
                        .font(theme::semibold(theme::font_size::CAPTION))
                        .color(theme::brand_fg()),
                );
                ui.label(RichText::new(calendar_name).font(theme::regular(theme::font_size::SMALL)).color(theme::fg1()));
                if !time_zone.is_empty() {
                    ui.label(
                        RichText::new(format!("· {time_zone}"))
                            .font(theme::regular(theme::font_size::MICRO))
                            .color(theme::fg3()),
                    );
                }
            });
            ui.add_space(5.0);
            if events.is_empty() {
                ui.label(
                    RichText::new("No Microsoft 365 events were returned for this bounded view.")
                        .font(theme::regular(theme::font_size::SMALL))
                        .color(theme::fg3()),
                );
                return;
            }

            ui.label(
                RichText::new(if state.can_edit {
                    "Microsoft 365 permits edits for this calendar, but Teamspace remote events are read-only."
                } else {
                    "This Microsoft 365 calendar is read-only in Teamspace."
                })
                .font(theme::regular(theme::font_size::MICRO))
                .color(theme::fg3()),
            );
            ui.add_space(4.0);

            for event in events {
                let is_selected = state
                    .selected_target
                    .as_ref()
                    .is_some_and(|target| *target == event.target);
                let response = egui::Frame::new()
                    .fill(if is_selected {
                        theme::surface()
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(if is_selected {
                        Stroke::new(1.0_f32, theme::brand_fg())
                    } else {
                        Stroke::NONE
                    })
                    .corner_radius(5.0)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&event.title).font(theme::semibold(theme::font_size::SMALL)).color(theme::fg1()));
                            if event.cancelled {
                                ui.label(RichText::new("Cancelled").font(theme::regular(theme::font_size::MICRO)).color(theme::fg3()));
                            }
                        });
                        ui.label(
                            RichText::new(format!("{} — {}", event.starts_at, event.ends_at))
                                .font(theme::regular(theme::font_size::MICRO))
                                .color(theme::fg3()),
                        );
                    });
                if clickable(ui, response.response, &event.title).clicked() {
                    selected = Some(event.target);
                }
            }
        });

    selected.is_some_and(|target| state.select(target))
}

/// Renders the detail view for a selected remote event and returns whether the
/// user explicitly requested the one provider-normalized meeting link. Opening
/// that link remains the app shell's responsibility.
pub(super) fn draw_selected_event_window(
    ctx: &egui::Context,
    state: &mut RemoteCalendarState,
) -> bool {
    let Some(event) = state.selected().cloned() else {
        return false;
    };
    let can_edit = state.can_edit;
    if take_escape(ctx) {
        state.clear_selection();
        return false;
    }

    let mut close_requested = false;
    let mut join_requested = false;
    // At most one remote detail dialog exists at once, and provider targets
    // intentionally do not implement `Hash` or expose their identifiers.
    let _ = modal::show(ctx, "remote_calendar_event", 410.0, |ui| {
        modal::heading(ui, "Microsoft 365 event");
        ui.add_space(4.0);
        ui.label(
            RichText::new(&event.title)
                .font(theme::semibold(theme::font_size::SUBTITLE))
                .color(theme::fg1()),
        );
        ui.label(
            RichText::new(format!("{} — {}", event.starts_at, event.ends_at))
                .font(theme::regular(theme::font_size::CAPTION))
                .color(theme::fg3()),
        );
        if !event.organizer.is_empty() {
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Organizer: {}", event.organizer))
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
        }
        if !event.location.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Location: {}", event.location))
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
        }
        ui.add_space(12.0);
        ui.label(
            RichText::new(if event.cancelled {
                "This event is cancelled in Microsoft 365."
            } else if event.meeting_available {
                "Microsoft 365 reports online meeting information for this event."
            } else {
                "No online meeting information was supplied for this event."
            })
            .font(theme::regular(theme::font_size::SMALL))
            .color(theme::fg3()),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(if can_edit {
                "Microsoft 365 permits edits for this calendar, but Teamspace has not enabled remote editing, deleting, or attendee changes."
            } else {
                "This remote event is read-only here. Editing, deleting, and attendee changes require separate provider contracts."
            })
            .font(theme::regular(theme::font_size::SMALL))
            .color(theme::fg3()),
        );
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Close").clicked() {
                close_requested = true;
            }
            let join_url_available = !event.cancelled && event.join_url.is_some();
            if ui
                .add_enabled(join_url_available, egui::Button::new("Join meeting"))
                .on_hover_text(if event.cancelled {
                    "Cancelled Microsoft 365 events cannot be joined from Teamspace"
                } else if event.meeting_available {
                    "Open this Microsoft 365 event's supplied meeting link in your system browser"
                } else {
                    "Microsoft 365 did not supply a safe meeting link for this event"
                })
                .clicked()
            {
                join_requested = true;
            }
        });
    });
    if close_requested {
        state.clear_selection();
    }
    join_requested
}
