use super::date::{CalendarDay, week_days};
use super::event::CalendarDate;
use super::names::{self as date, Ymd};
use super::persistence::{
    MAX_EVENT_HOUR, MAX_TITLE_BYTES, MIN_DURATION_MINUTES, max_duration_for_hour,
};
use super::{
    CalendarLayout, CalendarState, EventFilter, LocalEventMutationError, local_event_date_label,
};
use crate::ui::icon::{Icon, IconSize};
use crate::ui::modal;
use crate::ui::shortcuts::{singleline_submit_pressed, take_escape};
use crate::ui::theme::{self};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Layout, RichText, Stroke};

impl CalendarState {
    pub(super) fn draw_new_meeting_form(
        &mut self,
        ui: &mut egui::Ui,
        days: [CalendarDay; 5],
        toast: &mut ToastState,
    ) {
        if take_escape(ui.ctx()) {
            self.dismiss_new_meeting_form();
            return;
        }
        let focus_title = self.new_meeting_focus_requested;
        self.new_meeting_focus_requested = false;
        egui::Frame::new()
            .fill(theme::brand_tint())
            .stroke(Stroke::new(1.0_f32, theme::stroke()))
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.label(RichText::new("New meeting").strong().color(theme::fg1()));
                ui.add_space(6.0);
                let title_input = ui.add_sized(
                    [ui.available_width().min(420.0), 30.0],
                    egui::TextEdit::singleline(&mut self.meeting_title).hint_text("Meeting title"),
                );
                if focus_title {
                    title_input.request_focus();
                }
                let mut save_requested = singleline_submit_pressed(&title_input, ui);
                ui.add_space(7.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("When")
                            .font(theme::regular(theme::font_size::SMALL))
                            .color(theme::fg3()),
                    );
                    egui::ComboBox::from_id_salt("new_meeting_day")
                        .selected_text(format!(
                            "{} {:02}",
                            days[self.meeting_day.min(days.len() - 1)].weekday.name(),
                            days[self.meeting_day.min(days.len() - 1)].day
                        ))
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for (index, day) in days.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.meeting_day,
                                    index,
                                    format!("{} {:02}", day.weekday.name(), day.day),
                                );
                            }
                        });
                    egui::ComboBox::from_id_salt("new_meeting_time")
                        .selected_text(format!("{:02}:00", self.meeting_hour))
                        .width(84.0)
                        .show_ui(ui, |ui| {
                            for hour in 9..=15 {
                                ui.selectable_value(
                                    &mut self.meeting_hour,
                                    hour,
                                    format!("{:02}:00", hour),
                                );
                            }
                        });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            self.meeting_title.clear();
                            self.show_new_meeting = false;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Save").color(theme::fg_on_brand()),
                                )
                                .fill(theme::brand())
                                .corner_radius(5.0),
                            )
                            .clicked()
                        {
                            save_requested = true;
                        }
                    });
                });
                if save_requested {
                    if self.submit_meeting() {
                        toast.show(format!("Meeting added at {:02}:00", self.meeting_hour));
                    } else if self.meeting_title.trim().is_empty() {
                        toast.show_error("Give the meeting a title before saving.");
                    } else if self.meeting_title.trim().len() > MAX_TITLE_BYTES {
                        toast.show(format!(
                            "Meeting titles can be at most {MAX_TITLE_BYTES} bytes."
                        ));
                    } else {
                        toast.show_error("Couldn't save the meeting locally. Try again.");
                    }
                }
                ui.label(
                    RichText::new("Press Enter to save")
                        .font(theme::regular(theme::font_size::MICRO))
                        .color(theme::fg3()),
                );
            });
    }

    pub(super) fn draw_layout_menu(&mut self, ui: &mut egui::Ui, days: [CalendarDay; 5]) {
        for layout in [CalendarLayout::WorkWeek, CalendarLayout::Day] {
            if ui
                .selectable_label(self.layout == layout, layout.label())
                .clicked()
            {
                self.layout = layout;
            }
        }
        if self.layout == CalendarLayout::Day {
            ui.separator();
            ui.label(
                RichText::new("Show day")
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
            for (index, day) in days.iter().enumerate() {
                if ui
                    .selectable_label(
                        self.selected_day == index,
                        format!("{} {:02}", day.weekday.name(), day.day),
                    )
                    .clicked()
                {
                    self.selected_day = index;
                }
            }
        }
    }

    pub(super) fn draw_filter_menu(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        for filter in [EventFilter::All, EventFilter::Reference, EventFilter::Local] {
            if ui
                .selectable_label(self.event_filter == filter, filter.label())
                .clicked()
            {
                if self.set_event_filter(filter) {
                    toast.show(format!("Showing {}", filter.label().to_lowercase()));
                }
                ui.close();
            }
        }
    }

    pub(super) fn draw_selected_event_window(
        &mut self,
        ctx: &egui::Context,
        toast: &mut ToastState,
        call_active: bool,
    ) {
        let Some(selected_event) = self.selected_event.clone() else {
            return;
        };
        if take_escape(ctx) {
            self.clear_event_selection();
            return;
        }

        let mut open_meeting = false;
        let mut edit_requested = false;
        let mut delete_requested = false;
        let mut close_requested = false;
        let local_event_id = self.local_event_id_for_key(&selected_event.key);
        let title = selected_event.title;
        let time = selected_event.time;
        let modal_id = egui::Id::new(("calendar_event_details", selected_event.key));
        let _ = modal::show(ctx, modal_id, 390.0, |ui| {
            modal::heading(ui, "Event details");
            ui.add_space(4.0);
            ui.label(
                RichText::new(&title)
                    .font(theme::semibold(theme::font_size::SUBTITLE))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new(&time)
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg3()),
            );
            ui.add_space(12.0);
            ui.label(
                RichText::new(if local_event_id.is_some() {
                    "This local event stays on this device. You can edit or delete it here; no remote update or deletion is sent."
                } else {
                    "This reference event is view-only in this local preview. It does not join a real Microsoft Teams meeting."
                })
                .font(theme::regular(theme::font_size::SMALL))
                .color(theme::fg3()),
            );
            ui.add_space(12.0);
            if local_event_id.is_some() {
                ui.horizontal(|ui| {
                    if ui.button("Edit local event").clicked() {
                        edit_requested = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Delete local event").color(theme::fg_on_brand()),
                            )
                            .fill(theme::danger())
                            .corner_radius(6.0),
                        )
                        .clicked()
                    {
                        delete_requested = true;
                    }
                });
                ui.add_space(8.0);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_start_meeting = !call_active && !self.has_active_meeting();
                if ui
                    .add_enabled(
                        can_start_meeting,
                        egui::Button::new(
                            RichText::new("Open meeting locally").color(theme::fg_on_brand()),
                        )
                        .fill(theme::brand())
                        .corner_radius(6.0),
                    )
                    .on_hover_text(if can_start_meeting {
                        "Open this event in the local meeting preview"
                    } else {
                        "End the active local call or meeting before opening another"
                    })
                    .clicked()
                {
                    open_meeting = true;
                }
                if ui.button("Close").clicked() {
                    close_requested = true;
                }
            });
        });

        if edit_requested {
            if self.begin_editing_selected_local_event() {
                return;
            }
            self.clear_event_selection();
            toast.show_error(LocalEventMutationError::MissingLocalEvent.message());
            return;
        }
        if delete_requested {
            match self.delete_selected_local_event() {
                Ok(()) => toast.show("Deleted local event"),
                Err(error) => toast.show_error(error.message()),
            }
            return;
        }
        if open_meeting {
            if self.begin_local_event_meeting(local_event_id, &title, call_active) {
                toast.show(format!("Opened local meeting: {title}"));
                close_requested = true;
            } else {
                toast.show_error("End the active local call or meeting before opening another.");
            }
        }
        if close_requested {
            self.clear_event_selection();
        }
    }

    pub(super) fn draw_local_event_editor(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        let Some(event_id) = self.event_editor.as_ref().map(|editor| editor.event_id) else {
            return;
        };
        if take_escape(ctx) {
            self.event_editor = None;
            return;
        }

        let mut save_requested = false;
        let mut cancel_requested = false;
        {
            let editor = self
                .event_editor
                .as_mut()
                .expect("event editor was checked before rendering");
            let focus_requested = std::mem::take(&mut editor.focus_requested);
            let _ = modal::show(
                ctx,
                egui::Id::new(("calendar_local_event_editor", event_id)),
                420.0,
                |ui| {
                    modal::heading(ui, "Edit local event");
                    ui.add_space(8.0);
                    let title_input = ui.add_sized(
                        [ui.available_width(), 30.0],
                        egui::TextEdit::singleline(&mut editor.title).hint_text("Event title"),
                    );
                    if focus_requested {
                        title_input.request_focus();
                    }
                    save_requested = singleline_submit_pressed(&title_input, ui);
                    ui.add_space(10.0);
                    let date_picker_days = week_days(editor.date_picker_week_offset);
                    let can_move_date_picker_backward = editor.can_shift_date_picker_week(-1);
                    let can_move_date_picker_forward = editor.can_shift_date_picker_week(1);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Date")
                                .font(theme::regular(theme::font_size::SMALL))
                                .color(theme::fg3()),
                        );
                        let previous = ui.add_enabled(
                            can_move_date_picker_backward,
                            egui::Button::image(Icon::ChevronLeft.image(IconSize::S16)).small(),
                        );
                        previous.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                can_move_date_picker_backward,
                                "Previous week",
                            )
                        });
                        if previous
                            .on_hover_text("Previous week")
                            .on_disabled_hover_text("Previous week")
                            .clicked()
                        {
                            editor.shift_date_picker_week(-1);
                        }
                        ui.label(
                            RichText::new(date::week_of(Ymd::new(
                                date_picker_days[0].year,
                                date_picker_days[0].month,
                                date_picker_days[0].day,
                            )))
                            .font(theme::regular(theme::font_size::SMALL))
                            .color(theme::fg1()),
                        );
                        let next = ui.add_enabled(
                            can_move_date_picker_forward,
                            egui::Button::image(Icon::ChevronRight.image(IconSize::S16)).small(),
                        );
                        next.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                can_move_date_picker_forward,
                                "Next week",
                            )
                        });
                        if next
                            .on_hover_text("Next week")
                            .on_disabled_hover_text("Next week")
                            .clicked()
                        {
                            editor.shift_date_picker_week(1);
                        }
                    });
                    egui::ComboBox::from_id_salt(("calendar_local_event_date", event_id))
                        .selected_text(local_event_date_label(editor.date))
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for day in date_picker_days {
                                let date = CalendarDate::from_calendar_day(day);
                                ui.selectable_value(
                                    &mut editor.date,
                                    date,
                                    local_event_date_label(date),
                                );
                            }
                        });
                    ui.label(
                        RichText::new("Choose a weekday within six months of this event.")
                            .font(theme::regular(theme::font_size::MICRO))
                            .color(theme::fg3()),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Starts")
                                .font(theme::regular(theme::font_size::SMALL))
                                .color(theme::fg3()),
                        );
                        let hour_changed = ui
                            .add(
                                egui::DragValue::new(&mut editor.hour)
                                    .range(0..=MAX_EVENT_HOUR)
                                    .suffix(":00"),
                            )
                            .changed();
                        if hour_changed {
                            editor.duration_minutes = editor
                                .duration_minutes
                                .min(max_duration_for_hour(editor.hour));
                        }
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("Duration")
                                .font(theme::regular(theme::font_size::SMALL))
                                .color(theme::fg3()),
                        );
                        ui.add(
                            egui::DragValue::new(&mut editor.duration_minutes)
                                .range(MIN_DURATION_MINUTES..=max_duration_for_hour(editor.hour))
                                .suffix(" min"),
                        );
                    });
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Save").color(theme::fg_on_brand()),
                                )
                                .fill(theme::brand())
                                .corner_radius(6.0),
                            )
                            .clicked()
                        {
                            save_requested = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_requested = true;
                        }
                    });
                    ui.label(
                        RichText::new("Changes stay in this local calendar profile.")
                            .font(theme::regular(theme::font_size::MICRO))
                            .color(theme::fg3()),
                    );
                },
            );
        }

        if cancel_requested {
            self.event_editor = None;
            return;
        }
        if save_requested {
            match self.save_local_event_editor() {
                Ok(()) => toast.show("Updated local event"),
                Err(error) => toast.show_error(error.message()),
            }
        }
    }
}
