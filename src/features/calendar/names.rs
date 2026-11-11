//! Calendar weekday and month types with their English names and the date
//! formats the calendar shows.
//!
//! Weekday and month are real types, never English strings used as logic.

/// A day of the week, Monday first (the ISO order the calendar uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    pub(crate) const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    /// The weekday for a Monday-first index (0 = Monday).
    pub(crate) fn from_monday_index(index: u32) -> Self {
        Self::ALL[(index % 7) as usize]
    }

    pub(crate) const fn is_workday(self) -> bool {
        !matches!(self, Self::Saturday | Self::Sunday)
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Monday => "Monday",
            Self::Tuesday => "Tuesday",
            Self::Wednesday => "Wednesday",
            Self::Thursday => "Thursday",
            Self::Friday => "Friday",
            Self::Saturday => "Saturday",
            Self::Sunday => "Sunday",
        }
    }

    /// Column header of the month grid.
    pub(crate) const fn initial(self) -> &'static str {
        match self {
            Self::Monday => "M",
            Self::Tuesday | Self::Thursday => "T",
            Self::Wednesday => "W",
            Self::Friday => "F",
            Self::Saturday | Self::Sunday => "S",
        }
    }
}

/// A calendar month.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Month {
    January,
    February,
    March,
    April,
    May,
    June,
    July,
    August,
    September,
    October,
    November,
    December,
}

impl Month {
    /// The month for `1..=12`; anything else is a programming error, as it
    /// always was for the calendar.
    pub(crate) fn of(number: u32) -> Self {
        match number {
            1 => Self::January,
            2 => Self::February,
            3 => Self::March,
            4 => Self::April,
            5 => Self::May,
            6 => Self::June,
            7 => Self::July,
            8 => Self::August,
            9 => Self::September,
            10 => Self::October,
            11 => Self::November,
            12 => Self::December,
            _ => unreachable!("calendar month must be in the range 1..=12"),
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::January => "January",
            Self::February => "February",
            Self::March => "March",
            Self::April => "April",
            Self::May => "May",
            Self::June => "June",
            Self::July => "July",
            Self::August => "August",
            Self::September => "September",
            Self::October => "October",
            Self::November => "November",
            Self::December => "December",
        }
    }
}

/// A calendar date as plain numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ymd {
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
}

impl Ymd {
    pub(crate) const fn new(year: i32, month: u32, day: u32) -> Self {
        Self { year, month, day }
    }

    fn month_name(self) -> &'static str {
        Month::of(self.month).name()
    }
}

/// `Tuesday, November 03, 2026`.
pub(crate) fn long_date(date: Ymd, weekday: Weekday) -> String {
    format!(
        "{}, {} {:02}, {}",
        weekday.name(),
        date.month_name(),
        date.day,
        date.year
    )
}

/// Month grid header: `2026 November`.
pub(crate) fn month_year(year: i32, month: u32) -> String {
    format!("{year} {}", Month::of(month).name())
}

/// Date-picker caption for the week starting on `first`.
pub(crate) fn week_of(first: Ymd) -> String {
    format!("Week of {} {:02}", first.month_name(), first.day)
}

/// Heading for a Monday-to-Friday span with its ISO week number. A span that
/// crosses a year boundary names both years.
pub(crate) fn week_heading(first: Ymd, last: Ymd, week: u32) -> String {
    let (first_month, last_month) = (first.month_name(), last.month_name());
    let range = if first.year != last.year {
        format!(
            "{}, {first_month} {:02} – {}, {last_month} {:02}",
            first.year, first.day, last.year, last.day
        )
    } else if first.month == last.month {
        format!(
            "{}, {first_month} {:02}–{:02}",
            first.year, first.day, last.day
        )
    } else {
        format!(
            "{}, {first_month} {:02} – {last_month} {:02}",
            first.year, first.day, last.day
        )
    };
    format!("{range}  (Week {week})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_the_ones_the_calendar_always_showed() {
        assert_eq!(
            long_date(Ymd::new(2026, 11, 3), Weekday::Tuesday),
            "Tuesday, November 03, 2026"
        );
        assert_eq!(month_year(2026, 11), "2026 November");
        assert_eq!(week_of(Ymd::new(2026, 11, 2)), "Week of November 02");
        assert_eq!(
            week_heading(Ymd::new(2026, 11, 2), Ymd::new(2026, 11, 6), 45),
            "2026, November 02–06  (Week 45)"
        );
        assert_eq!(
            week_heading(Ymd::new(2026, 10, 26), Ymd::new(2026, 10, 30), 44),
            "2026, October 26–30  (Week 44)"
        );
        assert_eq!(
            week_heading(Ymd::new(2026, 10, 26), Ymd::new(2026, 11, 1), 44),
            "2026, October 26 – November 01  (Week 44)"
        );
    }

    #[test]
    fn a_week_across_new_year_names_both_years() {
        assert_eq!(
            week_heading(Ymd::new(2026, 12, 28), Ymd::new(2027, 1, 1), 53),
            "2026, December 28 – 2027, January 01  (Week 53)"
        );
    }

    #[test]
    fn weekdays_are_monday_first_and_the_weekend_is_not_a_workday() {
        assert_eq!(Weekday::from_monday_index(0), Weekday::Monday);
        assert_eq!(Weekday::from_monday_index(6), Weekday::Sunday);
        let work: Vec<_> = Weekday::ALL.iter().filter(|day| day.is_workday()).collect();
        assert_eq!(work.len(), 5);
        let initials: Vec<_> = Weekday::ALL.iter().map(|day| day.initial()).collect();
        assert_eq!(initials, ["M", "T", "W", "T", "F", "S", "S"]);
    }

    #[test]
    fn every_month_and_weekday_has_a_distinct_name() {
        let months: std::collections::HashSet<_> = (1..=12).map(|m| Month::of(m).name()).collect();
        assert_eq!(months.len(), 12);
        let days: std::collections::HashSet<_> = Weekday::ALL.iter().map(|d| d.name()).collect();
        assert_eq!(days.len(), 7);
    }
}
