//! A comparable, opaque message time.
//!
//! Graph timestamps reach features only as sanitized display text. New-message
//! detection needs to compare two of them, so the integration seam parses the
//! one fixed RFC 3339 shape Graph uses into an opaque, ordered value. Anything
//! else parses to `None`, which callers treat as "cannot prove it is newer".

/// Nanoseconds since the Unix epoch. Ordered; carries no display meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RemoteMessageInstant(i128);

impl RemoteMessageInstant {
    /// Parses `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (date, rest) = value.split_once(['T', 't'])?;
        let mut date = date.split('-');
        let year: i64 = number(date.next()?, 4)?;
        let month: i64 = number(date.next()?, 2)?;
        let day: i64 = number(date.next()?, 2)?;
        if date.next().is_some() || !(1..=12).contains(&month) {
            return None;
        }
        if !(1..=days_in_month(year, month)).contains(&day) {
            return None;
        }

        let split = rest.find(['Z', 'z', '+', '-'])?;
        let (clock, zone) = rest.split_at(split);
        let offset_seconds: i64 = match zone {
            "Z" | "z" => 0,
            zone => {
                let sign = if zone.starts_with('-') { -1 } else { 1 };
                let (hours, minutes) = zone[1..].split_once(':')?;
                let (hours, minutes): (i64, i64) = (number(hours, 2)?, number(minutes, 2)?);
                if hours > 23 || minutes > 59 {
                    return None;
                }
                sign * (hours * 3600 + minutes * 60)
            }
        };
        let (whole, fraction) = match clock.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (clock, None),
        };
        let mut parts = whole.split(':');
        let hour: i64 = number(parts.next()?, 2)?;
        let minute: i64 = number(parts.next()?, 2)?;
        let second: i64 = number(parts.next()?, 2)?;
        if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        let nanos: i128 = match fraction {
            None => 0,
            Some(digits) => {
                if digits.is_empty()
                    || digits.len() > 9
                    || !digits.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                let padded = format!("{digits:0<9}");
                padded.parse().ok()?
            }
        };
        let days = days_from_civil(year, month, day);
        let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
        Some(Self(i128::from(seconds) * 1_000_000_000 + nanos))
    }
}

impl RemoteMessageInstant {
    /// Nanoseconds since the Unix epoch, for a feature that formats the time
    /// in the user's own zone. It carries no zone of its own.
    pub(crate) const fn unix_nanos(self) -> i128 {
        self.0
    }
}

/// Exactly `width` ASCII digits.
fn number(text: &str, width: usize) -> Option<i64> {
    (text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

const fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::RemoteMessageInstant as Instant;

    fn parse(text: &str) -> Instant {
        Instant::parse(text).unwrap_or_else(|| panic!("{text} must parse"))
    }

    #[test]
    fn known_values_and_ordering() {
        assert_eq!(parse("1970-01-01T00:00:00Z").0, 0);
        assert_eq!(
            parse("2026-11-06T10:30:00Z").0,
            1_793_961_000 * 1_000_000_000
        );
        assert!(parse("2026-11-06T10:30:01Z") > parse("2026-11-06T10:30:00.999Z"));
        assert!(parse("2026-11-06T10:30:00.123456789Z") > parse("2026-11-06T10:30:00.12Z"));
        assert!(parse("2027-01-01T00:00:00Z") > parse("2026-12-31T23:59:59.999999999Z"));
    }

    #[test]
    fn unix_nanos_is_the_epoch_offset_in_nanoseconds() {
        assert_eq!(parse("1970-01-01T00:00:01Z").unix_nanos(), 1_000_000_000);
        assert_eq!(
            parse("1970-01-01T01:00:00+01:00").unix_nanos(),
            0,
            "an offset is folded into the instant"
        );
    }

    #[test]
    fn offsets_normalize_to_the_same_instant() {
        assert_eq!(
            parse("2026-11-06T11:30:00+01:00"),
            parse("2026-11-06T10:30:00Z")
        );
        assert_eq!(
            parse("2026-11-06T05:30:00-05:00"),
            parse("2026-11-06T10:30:00Z")
        );
    }

    #[test]
    fn leap_days_are_checked() {
        assert!(Instant::parse("2024-02-29T00:00:00Z").is_some());
        assert!(Instant::parse("2026-02-29T00:00:00Z").is_none());
        assert!(Instant::parse("2100-02-29T00:00:00Z").is_none());
    }

    #[test]
    fn anything_else_fails_closed() {
        for bad in [
            "",
            "Unknown time",
            "2026-11-06",
            "2026-11-06T10:30:00",
            "2026-13-06T10:30:00Z",
            "2026-11-32T10:30:00Z",
            "2026-11-06T24:30:00Z",
            "2026-11-06T10:60:00Z",
            "2026-11-06T10:30:00.Z",
            "2026-11-06T10:30:00.1234567890Z",
            "2026-11-06T10:30:00+0100",
            "2026-11-06T10:30Z",
            "26-11-06T10:30:00Z",
            "2026-11-06 10:30:00Z",
        ] {
            assert!(Instant::parse(bad).is_none(), "{bad:?}");
        }
    }
}
