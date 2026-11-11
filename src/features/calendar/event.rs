use super::date::CalendarDay;
use super::names::Weekday;

/// An absolute Gregorian date used by locally-created calendar events.
///
/// The UI can move between work weeks, but saved events must not depend on a
/// viewport-relative week offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CalendarDate {
    pub(crate) year: i32,
    pub(crate) month: u8,
    pub(crate) day: u8,
}

impl CalendarDate {
    pub(crate) const fn new(year: i32, month: u8, day: u8) -> Option<Self> {
        if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(Self { year, month, day })
    }

    pub(crate) const fn from_calendar_day(day: CalendarDay) -> Self {
        Self {
            year: day.year,
            month: day.month as u8,
            day: day.day as u8,
        }
    }

    pub(crate) const fn matches(self, day: CalendarDay) -> bool {
        self.year == day.year && self.month as u32 == day.month && self.day as u32 == day.day
    }

    pub(crate) fn weekday(self) -> Weekday {
        // Tomohiko Sakamoto's Gregorian weekday calculation, converted from
        // Sunday-first to the Monday-first order of `Weekday`.
        let month_offsets = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
        let year = if self.month < 3 {
            self.year - 1
        } else {
            self.year
        };
        let sunday_first = (year + year / 4 - year / 100
            + year / 400
            + month_offsets[(self.month - 1) as usize]
            + i32::from(self.day))
        .rem_euclid(7);
        Weekday::from_monday_index(((sunday_first + 6) % 7) as u32)
    }

    /// The local calendar renders a workweek grid, so durable local events
    /// must have a date that can be displayed, selected, and edited there.
    pub(crate) fn is_workweek_day(self) -> bool {
        self.weekday().is_workday()
    }

    /// Returns the preview-week offset that contains this date. The current
    /// fixture's Monday anchor is absolute, so saved events remain stable even
    /// after moving the viewport or restarting the app.
    pub(crate) fn preview_week_offset(self) -> i32 {
        let anchor = Self {
            year: 2026,
            month: 11,
            day: 2,
        };
        let day_delta = self.days_since(anchor);
        day_delta.div_euclid(7) as i32
    }

    fn days_since(self, other: Self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
            - days_from_civil(other.year, other.month, other.day)
    }
}

/// A local event is a durable domain record, separate from selection dialogs
/// and other calendar presentation state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalEvent {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) date: CalendarDate,
    pub(crate) hour: u8,
    pub(crate) duration_minutes: u16,
}

impl LocalEvent {
    pub(crate) fn key(&self) -> String {
        format!("local-{}", self.id)
    }

    pub(crate) fn time_label(&self) -> String {
        let end_minutes = u16::from(self.hour) * 60 + self.duration_minutes;
        let end_hour = (end_minutes / 60).min(23);
        let end_minute = end_minutes % 60;
        format!(
            "{} · {:02}:00–{:02}:{:02}",
            self.date.weekday().name(),
            self.hour,
            end_hour,
            end_minute
        )
    }
}

const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Days since a fixed civil epoch. This is a signed Gregorian conversion that
// avoids depending on the host timezone or a date crate for local previews.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i32::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i32::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468
}

#[cfg(test)]
mod tests {
    use super::{CalendarDate, LocalEvent};

    #[test]
    fn date_validation_handles_month_lengths_and_leap_years() {
        assert!(CalendarDate::new(2026, 2, 29).is_none());
        assert!(CalendarDate::new(2028, 2, 29).is_some());
        assert!(CalendarDate::new(2026, 13, 1).is_none());
        assert!(CalendarDate::new(2026, 11, 0).is_none());
    }

    #[test]
    fn local_event_uses_an_absolute_date_and_duration_for_its_label() {
        let event = LocalEvent {
            id: 7,
            title: "Design review".into(),
            date: CalendarDate::new(2026, 11, 5).expect("fixture date must be valid"),
            hour: 14,
            duration_minutes: 45,
        };

        assert_eq!(event.key(), "local-7");
        assert_eq!(event.time_label(), "Thursday · 14:00–14:45");
        assert_eq!(event.date.preview_week_offset(), 0);
    }

    #[test]
    fn preview_week_offsets_follow_absolute_dates_across_week_boundaries() {
        assert_eq!(
            CalendarDate::new(2026, 10, 30)
                .expect("fixture date must be valid")
                .preview_week_offset(),
            -1
        );
        assert_eq!(
            CalendarDate::new(2026, 11, 9)
                .expect("fixture date must be valid")
                .preview_week_offset(),
            1
        );
    }

    #[test]
    fn workweek_detection_excludes_weekend_dates() {
        assert!(
            CalendarDate::new(2026, 11, 2)
                .expect("fixture date must be valid")
                .is_workweek_day()
        );
        assert!(
            !CalendarDate::new(2026, 11, 7)
                .expect("fixture date must be valid")
                .is_workweek_day()
        );
    }
}
