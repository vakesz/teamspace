use super::date::{CalendarDay, adjacent_month_week_offset, heading, week_days};
use super::date::{PREVIEW_TODAY, iso_week_number, month_grid};
use super::names::{self as date, Weekday, Ymd};
use super::packing::pack_columns;
use super::{CalendarLayout, CalendarState, calendar_event_id, seeded_events};
use crate::ui::a11y::clickable;
use crate::ui::icon::{Icon, IconSize, icon_button};
use crate::ui::list_row::layout_truncated;
use crate::ui::modal;
use crate::ui::shortcuts::take_escape;
use crate::ui::theme::{self, font_size, space};
use crate::ui::toast::ToastState;
use eframe::egui::{self, Align, Color32, Layout, RichText, Stroke, Vec2};

impl CalendarState {
    pub(super) fn draw_month_header(&mut self, ui: &mut egui::Ui) {
        let shown = week_days(self.week_offset)[3];
        ui.horizontal(|ui| {
            ui.add_space(space::MD);
            ui.label(
                RichText::new(date::month_year(shown.year, shown.month))
                    .font(theme::regular(font_size::BODY))
                    .color(theme::fg1()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(space::SM);
                if icon_button(ui, Icon::ChevronDown, "Next month").clicked() {
                    self.change_week(
                        adjacent_month_week_offset(self.week_offset, true) - self.week_offset,
                    );
                }
                if icon_button(ui, Icon::ChevronUp, "Previous month").clicked() {
                    self.change_week(
                        adjacent_month_week_offset(self.week_offset, false) - self.week_offset,
                    );
                }
            });
        });
    }

    /// Month grid with an ISO week-number column. Today is a filled brand
    /// circle and the displayed work week is outlined.
    pub(super) fn draw_month_grid(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        let shown_week = week_days(self.week_offset);
        let current_week = week_days(0);
        let grid = month_grid(shown_week[3].year, shown_week[3].month);
        let margin = space::LG;
        let cell_w = ((ui.available_width() - 2.0 * margin) / 8.0).clamp(20.0, 34.0);
        let cell_h = 28.0;
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(cell_w * 8.0 + 2.0 * margin, cell_h * 7.0),
            egui::Sense::hover(),
        );
        let left = rect.left() + margin;
        let painter = ui.painter().clone();
        let caption = theme::regular(font_size::CAPTION);
        for (column, weekday) in Weekday::ALL.into_iter().map(Weekday::initial).enumerate() {
            painter.text(
                egui::pos2(
                    left + (column as f32 + 1.5) * cell_w,
                    rect.top() + cell_h * 0.5,
                ),
                egui::Align2::CENTER_CENTER,
                weekday,
                caption.clone(),
                theme::fg1(),
            );
        }
        for (row, week) in grid.iter().enumerate() {
            let top = rect.top() + (row as f32 + 1.0) * cell_h;
            let is_shown_week = week[0].year == shown_week[0].year
                && week[0].month == shown_week[0].month
                && week[0].day == shown_week[0].day;
            if is_shown_week {
                painter.rect(
                    egui::Rect::from_min_size(
                        egui::pos2(left + cell_w, top + 1.0),
                        Vec2::new(cell_w * 7.0, cell_h - 2.0),
                    ),
                    theme::radius::SM,
                    theme::brand_tint(),
                    Stroke::new(1.0_f32, theme::brand_fg()),
                    egui::StrokeKind::Inside,
                );
            }
            painter.text(
                egui::pos2(left + cell_w * 0.5, top + cell_h * 0.5),
                egui::Align2::CENTER_CENTER,
                iso_week_number(week[0].year, week[0].month, week[0].day).to_string(),
                caption.clone(),
                theme::fg3(),
            );
            for (column, cell) in week.iter().enumerate() {
                let center = egui::pos2(left + (column as f32 + 1.5) * cell_w, top + cell_h * 0.5);
                let cell_date = (cell.year, cell.month, cell.day);
                let is_today = cell_date == PREVIEW_TODAY;
                let workday = current_week
                    .iter()
                    .position(|day| (day.year, day.month, day.day) == cell_date);
                let is_selected_day = workday.is_some_and(|day_index| {
                    self.week_offset == 0
                        && self.layout == CalendarLayout::Day
                        && self.selected_day == day_index
                });
                let highlighted = is_today || is_selected_day;
                if highlighted {
                    painter.circle_filled(center, 11.0, theme::brand());
                }
                let in_month = cell.month == shown_week[3].month;
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    cell.day.to_string(),
                    caption.clone(),
                    if highlighted {
                        theme::surface()
                    } else if in_month {
                        theme::fg1()
                    } else {
                        theme::fg3()
                    },
                );
                let Some(day_index) = workday else {
                    continue;
                };
                let tooltip = if is_today {
                    "Go to today"
                } else {
                    "Show this day"
                };
                let response = ui
                    .interact(
                        egui::Rect::from_center_size(center, Vec2::splat(cell_h - 2.0)),
                        ui.id().with(("calendar_mini_day", day_index)),
                        egui::Sense::click(),
                    )
                    .on_hover_text(tooltip);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        format!("{tooltip}: {}", cell.day),
                    )
                });
                if response.clicked() {
                    if is_today {
                        self.focus_current_day();
                    } else {
                        self.focus_current_weekday(day_index);
                    }
                    toast.show(format!(
                        "Showing {}",
                        current_week[day_index].weekday.name()
                    ));
                }
            }
        }
    }

    pub(super) fn draw_calendar_list(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        ui.horizontal(|ui| {
            ui.add_space(space::SM);
            let calendars_open = self.calendars_open;
            if icon_button(
                ui,
                if calendars_open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                if calendars_open {
                    "Collapse My calendars"
                } else {
                    "Expand My calendars"
                },
            )
            .clicked()
            {
                self.toggle_calendars();
            }
            let mut all_calendars_visible = self.all_calendars_visible();
            if ui
                .checkbox(
                    &mut all_calendars_visible,
                    RichText::new("My calendars")
                        .font(theme::semibold(font_size::BODY))
                        .color(theme::fg1()),
                )
                .changed()
            {
                self.set_all_calendars_visible(all_calendars_visible);
                toast.show(if all_calendars_visible {
                    "Showing all calendars"
                } else {
                    "Hiding all calendars"
                });
            }
        });
        if !self.calendars_open {
            return;
        }
        let row_text = |text: &str| {
            RichText::new(text)
                .font(theme::regular(font_size::BODY))
                .color(theme::fg2())
        };
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            let mut primary_calendar_visible = self.primary_calendar_visible;
            if ui
                .checkbox(&mut primary_calendar_visible, row_text("Calendar"))
                .changed()
            {
                self.set_primary_calendar_visible(primary_calendar_visible);
                toast.show(if self.primary_calendar_visible {
                    "Showing Calendar"
                } else {
                    "Hiding Calendar"
                });
            }
        });
        let mut calendar_visibility_changed = false;
        for calendar in &mut self.additional_calendars {
            ui.horizontal(|ui| {
                ui.add_space(42.0);
                if ui
                    .checkbox(&mut calendar.visible, row_text(&calendar.name))
                    .changed()
                {
                    calendar_visibility_changed = true;
                    toast.show(if calendar.visible {
                        format!("Showing {}", calendar.name)
                    } else {
                        format!("Hiding {}", calendar.name)
                    });
                }
            });
        }
        if calendar_visibility_changed {
            self.mark_persistence_dirty();
        }
        ui.add_space(space::XL);
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            if ui
                .link(RichText::new("Show all").color(theme::brand_fg()))
                .clicked()
            {
                self.set_all_calendars_visible(true);
                toast.show("Showing all calendars");
            }
        });
    }

    /// Every event the grid shows for the displayed week, in day then start
    /// order, as plain rows for the keyboard-reachable agenda list.
    pub(super) fn agenda_entries(&self, days: [CalendarDay; 5]) -> Vec<AgendaEntry> {
        let mut entries = Vec::new();
        if !self.primary_calendar_visible {
            return entries;
        }
        if self.week_offset == 0 && self.event_filter.shows_reference() {
            for event in seeded_events() {
                entries.push(AgendaEntry {
                    key: event.id.into(),
                    title: split_event_label(event.title).0.trim().into(),
                    label: event.title.into(),
                    time: event.time.into(),
                    day: event.day,
                    start: event.hour,
                });
            }
        }
        if self.event_filter.shows_local() {
            for event in &self.local_events {
                let Some(day) = days.iter().position(|day| event.date.matches(*day)) else {
                    continue;
                };
                entries.push(AgendaEntry {
                    key: event.key(),
                    title: event.title.clone(),
                    label: event.title.clone(),
                    time: event.time_label(),
                    day,
                    start: f32::from(event.hour),
                });
            }
        }
        entries.sort_by(|a, b| {
            a.day
                .cmp(&b.day)
                .then(a.start.total_cmp(&b.start))
                .then_with(|| a.key.cmp(&b.key))
        });
        entries
    }

    /// A list of the displayed week's events. Unlike the painted grid, every
    /// row is an ordinary focusable button, so the week can be read and an
    /// event opened with the keyboard or a screen reader.
    pub(super) fn draw_agenda(&mut self, ui: &mut egui::Ui, toast: &mut ToastState) {
        let days = week_days(self.week_offset);
        let entries = self.agenda_entries(days);
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Agenda")
                    .font(theme::semibold(font_size::CAPTION))
                    .color(theme::fg3()),
            );
        });
        ui.add_space(space::XS);
        if entries.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label(
                    RichText::new("No events this week")
                        .font(theme::regular(font_size::CAPTION))
                        .color(theme::fg3()),
                );
            });
            return;
        }
        let mut requested = None;
        for entry in &entries {
            let selected = self
                .selected_event
                .as_ref()
                .is_some_and(|event| event.key == entry.key);
            // "day · time · title": shown truncated, and in full for hover and
            // assistive technology.
            let full = format!("{} · {}", entry.time, entry.title);
            // A custom row so the text sits at the left edge. It is allocated
            // at exactly the sidebar width: a wider row would grow the panel
            // and push the whole grid to the right.
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), AGENDA_ROW_HEIGHT),
                egui::Sense::click(),
            );
            let response = clickable(ui, response, &full).on_hover_text(&full);
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    selected,
                    &full,
                )
            });
            if ui.is_rect_visible(rect) {
                let row = rect.shrink2(egui::vec2(space::LG, 0.0));
                let fill = if selected {
                    Some(theme::brand_tint())
                } else if response.hovered() {
                    Some(theme::surface_hover())
                } else {
                    None
                };
                if let Some(fill) = fill {
                    ui.painter().rect_filled(
                        row,
                        egui::CornerRadius::same(theme::radius::SM),
                        fill,
                    );
                }
                if response.has_focus() {
                    ui.painter().rect_stroke(
                        row,
                        egui::CornerRadius::same(theme::radius::SM),
                        Stroke::new(1.0_f32, theme::brand_fg()),
                        egui::StrokeKind::Inside,
                    );
                }
                let text_left = row.left() + space::SM;
                let galley = layout_truncated(
                    ui,
                    &full,
                    theme::regular(font_size::CAPTION),
                    theme::fg1(),
                    row.right() - space::SM - text_left,
                );
                ui.painter().galley(
                    egui::pos2(text_left, row.center().y - galley.size().y / 2.0),
                    galley,
                    theme::fg1(),
                );
            }
            if response.clicked() {
                requested = Some(entry);
            }
        }
        if let Some(entry) = requested {
            self.select_event_by_key(&entry.key, &entry.label, entry.day, &entry.time);
            toast.show(format!("Selected {}", entry.title));
        }
    }

    pub(super) fn visible_days(&self, days: [CalendarDay; 5]) -> Vec<(usize, CalendarDay)> {
        match self.layout {
            CalendarLayout::WorkWeek => days.into_iter().enumerate().collect(),
            CalendarLayout::Day => vec![(
                self.selected_day.min(days.len() - 1),
                days[self.selected_day.min(days.len() - 1)],
            )],
        }
    }

    pub(super) fn heading(&self, days: [CalendarDay; 5]) -> String {
        match self.layout {
            CalendarLayout::WorkWeek => heading(days),
            CalendarLayout::Day => {
                let day = days[self.selected_day.min(days.len() - 1)];
                date::long_date(Ymd::new(day.year, day.month, day.day), day.weekday)
            }
        }
    }

    pub(super) fn draw_grid(
        &mut self,
        ui: &mut egui::Ui,
        visible_days: &[(usize, CalendarDay)],
        toast: &mut ToastState,
    ) {
        // One point is held back so the last day's closing border is drawn
        // inside the panel instead of on its clipped edge.
        let width = ui.available_width() - 1.0;
        let grid_left_offset = GRID_GUTTER_W;
        let col_w = day_column_width(width, visible_days.len());
        let today_column = (self.week_offset == 0)
            .then(|| {
                visible_days
                    .iter()
                    .position(|(_, day)| (day.year, day.month, day.day) == PREVIEW_TODAY)
            })
            .flatten();

        // Day headers stay fixed above the scrolling hour grid.
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(width, GRID_HEADER_H), egui::Sense::hover());
        let painter = ui.painter().clone();
        painter.rect_filled(header, 0.0, theme::surface());
        let grid_left = header.left() + grid_left_offset;
        for (column, (_, calendar_day)) in visible_days.iter().enumerate() {
            let left = grid_left + column as f32 * col_w;
            let is_today = today_column == Some(column);
            painter.line_segment(
                [
                    egui::pos2(left, header.top()),
                    egui::pos2(left, header.bottom()),
                ],
                Stroke::new(1.0_f32, theme::stroke_subtle()),
            );
            if is_today {
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(left, header.top()),
                        Vec2::new(col_w, 3.0),
                    ),
                    0.0,
                    theme::brand_fg(),
                );
            }
            let (number_color, caption_color) = if is_today {
                (theme::brand_fg(), theme::brand_fg())
            } else {
                (theme::fg1(), theme::fg3())
            };
            painter.text(
                egui::pos2(left + space::LG, header.top() + 20.0),
                egui::Align2::LEFT_CENTER,
                format!("{:02}", calendar_day.day),
                theme::semibold(font_size::TITLE),
                number_color,
            );
            painter.text(
                egui::pos2(left + space::LG, header.top() + 42.0),
                egui::Align2::LEFT_CENTER,
                calendar_day.weekday.name(),
                theme::regular(font_size::CAPTION),
                caption_color,
            );
        }
        painter.line_segment(
            [
                egui::pos2(header.left(), header.bottom()),
                egui::pos2(header.right(), header.bottom()),
            ],
            Stroke::new(1.0_f32, theme::stroke()),
        );

        let current_week = self.week_offset == 0;
        let event_filter = self.event_filter;
        let primary_calendar_visible = self.primary_calendar_visible;
        let local_events = self
            .local_events
            .iter()
            .filter(|event| event.date.preview_week_offset() == self.week_offset)
            .cloned()
            .collect::<Vec<_>>();

        // Start with the working day in view, like the Teams reference.
        let scroll_init_id = ui.id().with("calendar_grid_scroll_initialized");
        let first_frame = !ui.data(|data| data.get_temp::<bool>(scroll_init_id).unwrap_or(false));
        let mut scroll_area = egui::ScrollArea::vertical()
            .id_salt("calendar_grid_scroll")
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        if first_frame {
            ui.data_mut(|data| data.insert_temp(scroll_init_id, true));
            scroll_area =
                scroll_area.vertical_scroll_offset((INITIAL_SCROLL_HOUR - FIRST_HOUR) * GRID_ROW_H);
        }
        scroll_area.show(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(
                Vec2::new(width, HOUR_LABELS.len() as f32 * GRID_ROW_H),
                egui::Sense::hover(),
            );
            let painter = ui.painter().clone();
            painter.rect_filled(rect, 0.0, theme::surface_subtle());
            let columns_right = grid_left + col_w * visible_days.len() as f32;
            let work_top = rect.top() + (WORK_START_HOUR - FIRST_HOUR) * GRID_ROW_H;
            let work_bottom = rect.top() + (WORK_END_HOUR - FIRST_HOUR) * GRID_ROW_H;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(grid_left, work_top),
                    egui::pos2(columns_right, work_bottom),
                ),
                0.0,
                theme::surface(),
            );
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top()),
                    egui::pos2(grid_left, rect.bottom()),
                ),
                0.0,
                theme::surface(),
            );
            for (row, label) in HOUR_LABELS.iter().enumerate() {
                let y = rect.top() + row as f32 * GRID_ROW_H;
                painter.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    Stroke::new(1.0_f32, theme::stroke_subtle()),
                );
                painter.text(
                    egui::pos2(rect.left() + grid_left_offset * 0.5, y + 10.0),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    theme::regular(font_size::CAPTION),
                    theme::fg3(),
                );
            }
            for y in [work_top, work_bottom] {
                painter.line_segment(
                    [egui::pos2(grid_left, y), egui::pos2(rect.right(), y)],
                    Stroke::new(1.0_f32, theme::stroke()),
                );
            }
            for column in 0..=visible_days.len() {
                let x = grid_left + column as f32 * col_w;
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    Stroke::new(1.0_f32, theme::stroke_subtle()),
                );
            }

            // Events are collected first so overlapping ones in a day can be
            // packed side by side before anything is painted.
            let mut pending: Vec<PendingEvent> = Vec::new();
            let mut event = |event_key: &str,
                             day: usize,
                             placement: EventPlacement,
                             label: &str,
                             time: &str,
                             style: EventStyle| {
                let Some(column) = visible_days
                    .iter()
                    .position(|(source_day, _)| *source_day == day)
                else {
                    return;
                };
                pending.push(PendingEvent {
                    key: event_key.into(),
                    day,
                    column,
                    placement,
                    label: label.into(),
                    time: time.into(),
                    style,
                });
            };
            if primary_calendar_visible && current_week && event_filter.shows_reference() {
                for seeded_event in seeded_events() {
                    event(
                        seeded_event.id,
                        seeded_event.day,
                        EventPlacement {
                            start: seeded_event.hour - FIRST_HOUR,
                            hours: seeded_event.hours,
                        },
                        seeded_event.title,
                        seeded_event.time,
                        EventStyle {
                            fill: theme::sidebar_tint(),
                            accent: theme::fg3(),
                            recurring: seeded_event.recurring,
                        },
                    );
                }
            }
            if primary_calendar_visible && event_filter.shows_local() {
                for local_event in local_events {
                    let day = visible_days.iter().find_map(|(source_day, calendar_day)| {
                        local_event
                            .date
                            .matches(*calendar_day)
                            .then_some((*source_day, *calendar_day))
                    });
                    if let Some((day_index, _calendar_day)) = day {
                        let event_key = local_event.key();
                        let label = local_event.title.clone();
                        let time = local_event.time_label();
                        event(
                            &event_key,
                            day_index,
                            EventPlacement {
                                start: f32::from(local_event.hour) - FIRST_HOUR,
                                hours: f32::from(local_event.duration_minutes) / 60.0,
                            },
                            &label,
                            &time,
                            EventStyle {
                                fill: theme::brand_tint(),
                                accent: theme::brand_fg(),
                                recurring: false,
                            },
                        );
                    }
                }
            }
            drop(event);

            for column in 0..visible_days.len() {
                let indices: Vec<usize> = pending
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| event.column == column)
                    .map(|(index, _)| index)
                    .collect();
                let spans: Vec<(f32, f32)> =
                    indices.iter().map(|&index| pending[index].span()).collect();
                let slots = pack_columns(&spans);
                for (&index, slot) in indices.iter().zip(slots) {
                    let event = &pending[index];
                    let slot_w = (col_w - 6.0) / slot.columns as f32;
                    let event_rect = egui::Rect::from_min_size(
                        egui::pos2(
                            grid_left + column as f32 * col_w + 3.0 + slot.column as f32 * slot_w,
                            rect.top() + event.placement.start * GRID_ROW_H + 2.0,
                        ),
                        Vec2::new(
                            if slot.columns > 1 {
                                slot_w - 2.0
                            } else {
                                slot_w
                            },
                            event.height(),
                        ),
                    );
                    paint_event(&painter, ui, event_rect, &event.label, event.style);
                    let event_response = ui.interact(
                        event_rect,
                        calendar_event_id(&event.key),
                        egui::Sense::click(),
                    );
                    event_response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            format!("{}, {}", event.label, event.time),
                        )
                    });
                    if event_response.clicked() {
                        self.select_event_by_key(&event.key, &event.label, event.day, &event.time);
                        toast.show(format!("Selected {}", event.label));
                    }
                }
            }

            if let Some(column) = today_column {
                let now_y = rect.top() + (NOW_HOUR - FIRST_HOUR) * GRID_ROW_H;
                let today_left = grid_left + column as f32 * col_w;
                if column > 0 {
                    painter.extend(egui::Shape::dashed_line(
                        &[egui::pos2(grid_left, now_y), egui::pos2(today_left, now_y)],
                        Stroke::new(1.0_f32, theme::brand_fg()),
                        3.0,
                        3.0,
                    ));
                }
                painter.line_segment(
                    [
                        egui::pos2(today_left, now_y),
                        egui::pos2(today_left + col_w, now_y),
                    ],
                    Stroke::new(2.0_f32, theme::brand_fg()),
                );
                painter.circle_filled(egui::pos2(today_left, now_y), 5.0, theme::brand());
            }
        });
    }

    pub(super) fn draw_picker(&mut self, ctx: &egui::Context, toast: &mut ToastState) {
        if !self.picker_open {
            return;
        }
        if take_escape(ctx) {
            self.picker_open = false;
            return;
        }
        let mut selection = None;
        let mut close_requested = false;
        let _ = modal::show(ctx, "calendar_picker", 390.0, |ui| {
            modal::heading(ui, "Add a calendar");
            ui.add_space(4.0);
            ui.label(
                RichText::new("Choose a local preview calendar to add.")
                    .font(theme::regular(theme::font_size::CAPTION))
                    .color(theme::fg1()),
            );
            ui.label(
                RichText::new("Added calendars are kept locally for this profile.")
                    .font(theme::regular(theme::font_size::SMALL))
                    .color(theme::fg3()),
            );
            ui.add_space(10.0);
            for calendar in ["Holidays", "Birthdays", "Project milestones"] {
                if ui
                    .add(egui::Button::image_and_text(
                        Icon::Add.image(IconSize::S16),
                        calendar,
                    ))
                    .clicked()
                {
                    selection = Some(calendar);
                }
            }
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    close_requested = true;
                }
            });
        });
        if let Some(calendar) = selection {
            let added = self.add_calendar(calendar);
            toast.show(if added {
                format!("Added {calendar} to My calendars")
            } else {
                format!("{calendar} is already in My calendars")
            });
            close_requested = true;
        }
        if close_requested {
            self.picker_open = false;
        }
    }
}

const GRID_GUTTER_W: f32 = 50.0;

/// Width of each day column: the space left of the hour-label gutter divided
/// evenly, so every visible day fits at any panel width.
fn day_column_width(width: f32, days: usize) -> f32 {
    ((width - GRID_GUTTER_W) / days.max(1) as f32).max(0.0)
}
const GRID_HEADER_H: f32 = 56.0;
const GRID_ROW_H: f32 = 32.0;
const MIN_EVENT_HEIGHT: f32 = 20.0;
const AGENDA_ROW_HEIGHT: f32 = 26.0;
/// The grid starts at 00:00 and shows one row per hour through 23:00, the
/// same range the local event editor accepts.
const FIRST_HOUR: f32 = 0.0;
const WORK_START_HOUR: f32 = 8.0;
const WORK_END_HOUR: f32 = 17.0;
const INITIAL_SCROLL_HOUR: f32 = 7.0;
/// Fixed preview "now" (13:20 on the preview today), see `PREVIEW_TODAY`.
const NOW_HOUR: f32 = 13.3;
const HOUR_LABELS: [&str; 24] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23",
];

pub(super) fn toolbar_text(text: &str) -> RichText {
    RichText::new(text)
        .font(theme::regular(font_size::BODY))
        .color(theme::fg1())
}

/// Vertical placement of an event, in grid rows (hours) from the first hour.
#[derive(Clone, Copy)]
struct EventPlacement {
    start: f32,
    hours: f32,
}

/// One row of the agenda list.
pub(super) struct AgendaEntry {
    pub(super) key: String,
    pub(super) title: String,
    /// The label the grid passes to selection (organizer text included).
    pub(super) label: String,
    pub(super) time: String,
    pub(super) day: usize,
    pub(super) start: f32,
}

/// An event waiting to be packed and painted.
struct PendingEvent {
    key: String,
    day: usize,
    /// Index into the visible day columns.
    column: usize,
    placement: EventPlacement,
    label: String,
    time: String,
    style: EventStyle,
}

impl PendingEvent {
    /// The painted height: short events are still tall enough to read.
    fn height(&self) -> f32 {
        (self.placement.hours * GRID_ROW_H - 3.0).max(MIN_EVENT_HEIGHT)
    }

    /// The rows the painted event covers, so packing matches what overlaps
    /// on screen rather than the nominal duration.
    fn span(&self) -> (f32, f32) {
        (
            self.placement.start,
            self.placement.start + (self.height() + 3.0) / GRID_ROW_H,
        )
    }
}

#[derive(Clone, Copy)]
struct EventStyle {
    fill: Color32,
    accent: Color32,
    recurring: bool,
}

/// Splits a seeded event label into its title and organizer parts. The
/// organizer follows the title after a newline or a double space.
fn split_event_label(label: &str) -> (&str, &str) {
    label
        .split_once('\n')
        .or_else(|| label.split_once("  "))
        .map_or((label, ""), |(title, organizer)| (title, organizer.trim()))
}

fn paint_event(
    painter: &egui::Painter,
    ui: &egui::Ui,
    rect: egui::Rect,
    label: &str,
    style: EventStyle,
) {
    painter.rect_filled(rect, theme::radius::SM, style.fill);
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
        egui::CornerRadius {
            nw: theme::radius::SM,
            sw: theme::radius::SM,
            ne: 0,
            se: 0,
        },
        style.accent,
    );
    let clipped = painter.with_clip_rect(rect);
    let (title, organizer) = split_event_label(label);
    let origin = rect.left_top() + egui::vec2(space::MD + 3.0, 3.0);
    let title_galley = painter.layout_no_wrap(
        title.to_owned(),
        theme::semibold(font_size::CAPTION),
        theme::fg1(),
    );
    let title_width = title_galley.size().x;
    let title_height = title_galley.size().y;
    clipped.galley(origin, title_galley, theme::fg1());
    if !organizer.is_empty() {
        let tall = rect.height() >= 2.0 * title_height + 6.0;
        let organizer_pos = if tall {
            origin + egui::vec2(0.0, title_height)
        } else {
            origin + egui::vec2(title_width + space::SM, 0.0)
        };
        clipped.text(
            organizer_pos,
            egui::Align2::LEFT_TOP,
            organizer,
            theme::regular(font_size::CAPTION),
            theme::fg2(),
        );
    }
    if style.recurring {
        let side = IconSize::S16.px();
        Icon::Repeat
            .image(IconSize::S16)
            .tint(theme::fg3())
            .paint_at(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(rect.right() - side - 4.0, rect.top() + 3.0),
                    Vec2::splat(side),
                ),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::{GRID_GUTTER_W, day_column_width};

    #[test]
    fn day_columns_always_fit_inside_the_panel() {
        for width in [320.0_f32, 600.0, 800.0, 1098.0, 2000.0] {
            for days in [1_usize, 5] {
                let total = GRID_GUTTER_W + day_column_width(width, days) * days as f32;
                assert!(total <= width + 0.01, "{days} days overflow {width}");
            }
        }
        assert_eq!(day_column_width(10.0, 5), 0.0);
    }

    use super::{FIRST_HOUR, HOUR_LABELS};

    #[test]
    fn grid_covers_every_hour_the_event_editor_accepts() {
        assert_eq!(FIRST_HOUR, 0.0);
        assert_eq!(HOUR_LABELS.len(), 24);
        assert_eq!(HOUR_LABELS[0], "0");
        assert_eq!(HOUR_LABELS[23], "23");
    }
}
