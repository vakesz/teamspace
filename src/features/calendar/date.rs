use super::names::{self as date, Weekday, Ymd};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CalendarDay {
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) weekday: Weekday,
}

pub(crate) fn week_days(week_offset: i32) -> [CalendarDay; 5] {
    let (mut year, mut month, mut day) = add_days(2026, 11, 2, week_offset * 7);
    std::array::from_fn(|index| {
        if index > 0 {
            (year, month, day) = add_days(year, month, day, 1);
        }
        CalendarDay {
            year,
            month,
            day,
            weekday: Weekday::from_monday_index(index as u32),
        }
    })
}

pub(crate) fn heading(days: [CalendarDay; 5]) -> String {
    let (first, last) = (days[0], days[4]);
    date::week_heading(
        Ymd::new(first.year, first.month, first.day),
        Ymd::new(last.year, last.month, last.day),
        iso_week_number(first.year, first.month, first.day),
    )
}

/// The fixed "today" of the local preview calendar (Tuesday of week 45).
pub(crate) const PREVIEW_TODAY: (i32, u32, u32) = (2026, 11, 3);

/// One cell of the sidebar month grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MonthCell {
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
}

/// Monday-first weekday index (0 = Monday) of a proleptic Gregorian date.
pub(crate) fn weekday_index(year: i32, month: u32, day: u32) -> u32 {
    const OFFSETS: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let sunday_first =
        (y + y / 4 - y / 100 + y / 400 + OFFSETS[month as usize - 1] + day as i32).rem_euclid(7);
    ((sunday_first + 6) % 7) as u32
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn weeks_in_iso_year(year: i32) -> u32 {
    let jan_first = weekday_index(year, 1, 1);
    if jan_first == 3 || (is_leap_year(year) && jan_first == 2) {
        53
    } else {
        52
    }
}

/// ISO 8601 week number of a date.
pub(crate) fn iso_week_number(year: i32, month: u32, day: u32) -> u32 {
    let ordinal: u32 = (1..month).map(|m| days_in_month(year, m)).sum::<u32>() + day;
    let weekday = weekday_index(year, month, day) + 1;
    let week = (ordinal + 10).saturating_sub(weekday) / 7;
    if week < 1 {
        weeks_in_iso_year(year - 1)
    } else if week > weeks_in_iso_year(year) {
        1
    } else {
        week
    }
}

/// Six Monday-first week rows covering `month`, including adjacent-month days.
pub(crate) fn month_grid(year: i32, month: u32) -> [[MonthCell; 7]; 6] {
    let lead = weekday_index(year, month, 1) as i32;
    let (mut y, mut m, mut d) = add_days(year, month, 1, -lead);
    std::array::from_fn(|_| {
        std::array::from_fn(|_| {
            let cell = MonthCell {
                year: y,
                month: m,
                day: d,
            };
            (y, m, d) = add_days(y, m, d, 1);
            cell
        })
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(if month <= 2 { year - 1 } else { year });
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The week offset whose Monday is `year-month-day` (which must be a Monday
/// or any day; the containing week's offset is returned).
fn week_offset_containing(year: i32, month: u32, day: u32) -> i32 {
    let monday = days_from_civil(year, month, day) - i64::from(weekday_index(year, month, day));
    let reference = days_from_civil(2026, 11, 2);
    ((monday - reference) / 7) as i32
}

/// The week offset one calendar month before or after the month shown by
/// `week_offset`. The shown month is the month of the week's Thursday, so the
/// target is the week containing the first Thursday of the neighbouring month;
/// the header month then always changes by exactly one.
pub(crate) fn adjacent_month_week_offset(week_offset: i32, forward: bool) -> i32 {
    let shown = week_days(week_offset)[3];
    let (year, month) = match (forward, shown.month) {
        (true, 12) => (shown.year + 1, 1),
        (true, month) => (shown.year, month + 1),
        (false, 1) => (shown.year - 1, 12),
        (false, month) => (shown.year, month - 1),
    };
    let first_thursday = 1 + (3 + 7 - weekday_index(year, month, 1)) % 7;
    week_offset_containing(year, month, first_thursday)
}

fn add_days(mut year: i32, mut month: u32, mut day: u32, offset: i32) -> (i32, u32, u32) {
    if offset >= 0 {
        for _ in 0..offset {
            if day == days_in_month(year, month) {
                day = 1;
                if month == 12 {
                    month = 1;
                    year += 1;
                } else {
                    month += 1;
                }
            } else {
                day += 1;
            }
        }
    } else {
        for _ in offset..0 {
            if day == 1 {
                if month == 1 {
                    month = 12;
                    year -= 1;
                } else {
                    month -= 1;
                }
                day = days_in_month(year, month);
            } else {
                day -= 1;
            }
        }
    }
    (year, month, day)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => unreachable!("calendar month must be in the range 1..=12"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_weeks_match_the_reference_calendar() {
        assert_eq!(iso_week_number(2026, 11, 2), 45);
        assert_eq!(iso_week_number(2026, 11, 8), 45);
        assert_eq!(iso_week_number(2026, 1, 1), 1);
        assert_eq!(iso_week_number(2021, 1, 1), 53);
        assert_eq!(iso_week_number(2024, 12, 30), 1);
    }

    #[test]
    fn heading_week_number_is_iso_across_year_boundaries() {
        let heading_for = |offset| heading(week_days(offset));
        assert!(heading_for(0).ends_with("(Week 45)"));
        // Week of 2026-12-28 is ISO week 53; the next is week 1 of 2027.
        assert!(heading_for(8).ends_with("(Week 53)"), "{}", heading_for(8));
        assert!(heading_for(9).ends_with("(Week 1)"), "{}", heading_for(9));
        assert!(
            heading_for(-44).ends_with("(Week 1)"),
            "{}",
            heading_for(-44)
        );
    }

    #[test]
    fn heading_names_both_years_when_the_week_crosses_new_year() {
        let heading_for = |offset| heading(week_days(offset));
        assert_eq!(heading_for(0), "2026, November 02–06  (Week 45)");
        // Monday 2026-12-28 to Friday 2027-01-01 crosses the year boundary.
        assert_eq!(
            heading_for(8),
            "2026, December 28 – 2027, January 01  (Week 53)"
        );
        assert_eq!(week_days(0)[1].weekday.name(), "Tuesday");
    }

    #[test]
    fn month_steps_change_the_shown_month_by_exactly_one() {
        for start in -60..60 {
            let shown = week_days(start)[3];
            let forward = week_days(adjacent_month_week_offset(start, true))[3];
            let back = week_days(adjacent_month_week_offset(start, false))[3];
            let index = |day: CalendarDay| day.year * 12 + day.month as i32;
            assert_eq!(index(forward), index(shown) + 1, "forward from {start}");
            assert_eq!(index(back), index(shown) - 1, "back from {start}");
        }
    }

    #[test]
    fn weekdays_are_monday_first() {
        assert_eq!(weekday_index(2026, 11, 2), 0);
        assert_eq!(weekday_index(2026, 11, 3), 1);
        assert_eq!(weekday_index(2026, 11, 8), 6);
    }

    #[test]
    fn november_2026_grid_matches_the_preview_layout() {
        let grid = month_grid(2026, 11);
        assert_eq!((grid[0][0].month, grid[0][0].day), (10, 26));
        assert_eq!((grid[1][0].month, grid[1][0].day), (11, 2));
        assert_eq!((grid[5][0].month, grid[5][0].day), (11, 30));
        assert_eq!((grid[5][6].month, grid[5][6].day), (12, 6));
    }
}
