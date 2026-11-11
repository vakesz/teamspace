//! Microsoft 365 times in the user's own time zone.
//!
//! The integration seam hands features an opaque, zone-free
//! [`RemoteMessageInstant`]. This is the one place that turns it into text:
//! the user's local date and a 24-hour clock, with a relative day for recent
//! and upcoming times, in the style the local preview already uses
//! (`Today 11:25`, `Yesterday 13:35`, `Wednesday 12:10`, `Oct 14, 11:09`).

use crate::integrations::RemoteMessageInstant;
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

/// "Now" and the zone to show times in. Production reads the system clock and
/// zone; tests build a fixed one.
#[derive(Clone, Debug)]
pub(crate) struct Clock {
    now: Timestamp,
    zone: TimeZone,
}

impl Clock {
    /// Reads the system clock and the user's time zone (UTC when the OS does
    /// not say).
    #[cfg(not(test))]
    pub(crate) fn system() -> Self {
        Self::fixed(Timestamp::now(), TimeZone::system())
    }

    pub(crate) const fn fixed(now: Timestamp, zone: TimeZone) -> Self {
        Self { now, zone }
    }

    fn today(&self) -> Date {
        self.now.to_zoned(self.zone.clone()).date()
    }

    fn zoned(&self, instant: RemoteMessageInstant) -> Option<Zoned> {
        let stamp = Timestamp::from_nanosecond(instant.unix_nanos()).ok()?;
        Some(stamp.to_zoned(self.zone.clone()))
    }
}

/// The clock labels are built with. Tests see a fixed clock (Friday
/// 2026-11-06 12:00 UTC, shown in UTC) so results never depend on the host.
pub(crate) fn clock() -> Clock {
    #[cfg(test)]
    {
        Clock::fixed(
            "2026-11-06T12:00:00Z".parse().expect("fixed time parses"),
            TimeZone::UTC,
        )
    }
    #[cfg(not(test))]
    {
        Clock::system()
    }
}

/// `Today 17:14`, `Yesterday 09:05`, `Wednesday 12:10`, `Oct 14, 11:09` or, in
/// another year, `Oct 14, 2025 11:09`. An instant the clock cannot place gives
/// `None`.
pub(crate) fn label(instant: RemoteMessageInstant, clock: &Clock) -> Option<String> {
    let zoned = clock.zoned(instant)?;
    Some(format!(
        "{} {}",
        day_label(zoned.date(), clock.today(), true),
        clock_time(&zoned)
    ))
}

/// The label for a Remote value, or `fallback` when it has no instant.
pub(crate) fn label_or(
    instant: Option<RemoteMessageInstant>,
    fallback: &str,
    clock: &Clock,
) -> String {
    instant
        .and_then(|instant| label(instant, clock))
        .unwrap_or_else(|| fallback.to_owned())
}

/// A start and end for an event: `("Today 09:00", "10:00")`, with the end
/// carrying its own day only when it falls on another local date. `None`
/// unless both instants can be placed.
pub(crate) fn range(
    start: Option<RemoteMessageInstant>,
    end: Option<RemoteMessageInstant>,
    clock: &Clock,
) -> Option<(String, String)> {
    let start = clock.zoned(start?)?;
    let end = clock.zoned(end?)?;
    let today = clock.today();
    let from = format!(
        "{} {}",
        day_label(start.date(), today, true),
        clock_time(&start)
    );
    let to = if end.date() == start.date() {
        clock_time(&end)
    } else {
        format!(
            "{} {}",
            day_label(end.date(), today, true),
            clock_time(&end)
        )
    };
    Some((from, to))
}

fn clock_time(zoned: &Zoned) -> String {
    format!("{:02}:{:02}", zoned.hour(), zoned.minute())
}

/// The relative or calendar name of `date` as seen from `today`.
fn day_label(date: Date, today: Date, relative: bool) -> String {
    let days = today
        .until(date)
        .map(|span| span.get_days())
        .unwrap_or(i32::MAX);
    if relative {
        match days {
            0 => return "Today".into(),
            -1 => return "Yesterday".into(),
            1 => return "Tomorrow".into(),
            -6..=-2 | 2..=6 => return weekday_name(date.weekday()).into(),
            _ => {}
        }
    }
    let month = MONTHS[usize::from(date.month().unsigned_abs()) - 1];
    if date.year() == today.year() {
        format!("{month} {:02},", date.day())
    } else {
        format!("{month} {:02}, {}", date.day(), date.year())
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const fn weekday_name(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Monday => "Monday",
        Weekday::Tuesday => "Tuesday",
        Weekday::Wednesday => "Wednesday",
        Weekday::Thursday => "Thursday",
        Weekday::Friday => "Friday",
        Weekday::Saturday => "Saturday",
        Weekday::Sunday => "Sunday",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::{self, TimeZone};

    fn instant(value: &str) -> RemoteMessageInstant {
        RemoteMessageInstant::parse(value).expect("test timestamp parses")
    }

    fn clock_in(zone: TimeZone) -> Clock {
        // Friday 2026-11-06, 12:00 UTC.
        Clock::fixed("2026-11-06T12:00:00Z".parse().expect("now"), zone)
    }

    #[test]
    fn same_day_and_yesterday_are_relative() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(
            label(instant("2026-11-06T17:14:00Z"), &clock).as_deref(),
            Some("Today 17:14")
        );
        assert_eq!(
            label(instant("2026-11-05T09:05:00.1234567Z"), &clock).as_deref(),
            Some("Yesterday 09:05")
        );
    }

    #[test]
    fn the_last_week_uses_the_weekday_name() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(
            label(instant("2026-11-04T08:00:00Z"), &clock).as_deref(),
            Some("Wednesday 08:00")
        );
        assert_eq!(
            label(instant("2026-10-31T23:59:00Z"), &clock).as_deref(),
            Some("Saturday 23:59")
        );
        // Seven days back is no longer unambiguous: use the date.
        assert_eq!(
            label(instant("2026-10-30T10:00:00Z"), &clock).as_deref(),
            Some("Oct 30, 10:00")
        );
    }

    #[test]
    fn older_times_use_the_date_and_add_the_year_only_when_it_differs() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(
            label(instant("2026-10-05T09:10:00Z"), &clock).as_deref(),
            Some("Oct 05, 09:10")
        );
        assert_eq!(
            label(instant("2025-12-31T09:10:00Z"), &clock).as_deref(),
            Some("Dec 31, 2025 09:10")
        );
    }

    #[test]
    fn upcoming_times_are_relative_too() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(
            label(instant("2026-11-07T10:00:00Z"), &clock).as_deref(),
            Some("Tomorrow 10:00")
        );
        assert_eq!(
            label(instant("2026-11-09T10:00:00Z"), &clock).as_deref(),
            Some("Monday 10:00")
        );
        assert_eq!(
            label(instant("2026-11-20T10:00:00Z"), &clock).as_deref(),
            Some("Nov 20, 10:00")
        );
    }

    #[test]
    fn the_local_zone_moves_the_day_and_the_clock() {
        let budapest = TimeZone::get("Europe/Budapest").expect("named zone");
        let clock = clock_in(budapest);
        // 23:30 UTC on Thursday is 00:30 on Friday in Budapest (UTC+1).
        assert_eq!(
            label(instant("2026-11-05T23:30:00Z"), &clock).as_deref(),
            Some("Today 00:30")
        );
        // A -05:00 offset on the wire is the same instant as 22:00 UTC.
        assert_eq!(
            label(instant("2026-11-05T17:00:00-05:00"), &clock).as_deref(),
            Some("Yesterday 23:00")
        );
        let fixed = clock_in(TimeZone::fixed(tz::offset(-8)));
        assert_eq!(
            label(instant("2026-11-06T03:00:00Z"), &fixed).as_deref(),
            Some("Yesterday 19:00")
        );
    }

    #[test]
    fn the_new_year_and_daylight_saving_are_handled_by_the_zone() {
        let new_york = TimeZone::get("America/New_York").expect("named zone");
        let clock = Clock::fixed(
            "2027-01-01T12:00:00Z".parse().expect("now"),
            new_york.clone(),
        );
        assert_eq!(
            label(instant("2026-12-31T23:00:00Z"), &clock).as_deref(),
            Some("Yesterday 18:00")
        );
        // A summer time keeps its offset (EDT, UTC-4) rather than today's.
        assert_eq!(
            label(instant("2026-07-01T16:00:00Z"), &clock).as_deref(),
            Some("Jul 01, 2026 12:00")
        );
    }

    #[test]
    fn events_show_one_day_when_they_start_and_end_on_it() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(
            range(
                Some(instant("2026-11-06T09:00:00Z")),
                Some(instant("2026-11-06T10:30:00Z")),
                &clock
            ),
            Some(("Today 09:00".into(), "10:30".into()))
        );
        assert_eq!(
            range(
                Some(instant("2026-11-06T22:00:00Z")),
                Some(instant("2026-11-07T01:00:00Z")),
                &clock
            ),
            Some(("Today 22:00".into(), "Tomorrow 01:00".into()))
        );
    }

    #[test]
    fn an_unplaced_time_falls_back_to_the_provider_text() {
        let clock = clock_in(TimeZone::UTC);
        assert_eq!(label_or(None, "2026-11-06 odd", &clock), "2026-11-06 odd");
        assert_eq!(
            range(None, Some(instant("2026-11-06T10:30:00Z")), &clock),
            None
        );
        assert_eq!(
            label_or(Some(instant("2026-11-06T10:30:00Z")), "ignored", &clock),
            "Today 10:30"
        );
    }

    #[test]
    fn the_test_clock_is_fixed_and_utc() {
        assert_eq!(
            label(instant("2026-11-06T09:41:00Z"), &clock()).as_deref(),
            Some("Today 09:41")
        );
    }
}
