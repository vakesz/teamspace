//! Durable, local-only storage for calendar-owned user data.
//!
//! Calendar presentation state stays in the feature UI. This module stores
//! only versioned domain data and hides file-system safety details behind a
//! small concrete interface.

use super::event::CalendarDate;
use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Version 2 establishes the same-day duration and workweek-date invariants
/// used by the calendar grid and local preview. V1 snapshots are normalized
/// atomically when first loaded where that can preserve their meaning.
pub(crate) const CALENDAR_SCHEMA_VERSION: u32 = 2;
const LEGACY_CALENDAR_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_EVENTS: usize = 10_000;
const MAX_ADDITIONAL_CALENDARS: usize = 512;
pub(crate) const MAX_TITLE_BYTES: usize = 4 * 1024;
const MAX_CALENDAR_NAME_BYTES: usize = 512;
pub(crate) const MIN_DURATION_MINUTES: u16 = 5;
/// Local events deliberately remain within their start date. The last valid
/// end time is 23:59; representing an event that crosses midnight would also
/// require a second-day grid segment and an explicit end date.
pub(crate) const MAX_DURATION_MINUTES: u16 = 24 * 60 - 1;
pub(crate) const MAX_EVENT_HOUR: u8 = 23;

/// The durable portion of the local calendar preview. Selections, filters,
/// active meetings, forms, and other transient UI values are excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredCalendar {
    pub(crate) schema_version: u32,
    pub(crate) next_local_event_id: u64,
    #[serde(default)]
    pub(crate) events: Vec<StoredEvent>,
    #[serde(default = "default_primary_calendar_visible")]
    pub(crate) primary_calendar_visible: bool,
    #[serde(default = "default_calendars_open")]
    pub(crate) calendars_open: bool,
    #[serde(default)]
    pub(crate) additional_calendars: Vec<StoredAdditionalCalendar>,
}

impl Default for StoredCalendar {
    fn default() -> Self {
        Self {
            schema_version: CALENDAR_SCHEMA_VERSION,
            next_local_event_id: 1,
            events: Vec::new(),
            primary_calendar_visible: true,
            calendars_open: true,
            additional_calendars: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredEvent {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u8,
    pub(crate) duration_minutes: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredAdditionalCalendar {
    pub(crate) name: String,
    #[serde(default = "default_calendar_visible")]
    pub(crate) visible: bool,
}

const fn default_primary_calendar_visible() -> bool {
    true
}

const fn default_calendars_open() -> bool {
    true
}

const fn default_calendar_visible() -> bool {
    true
}

/// A concrete local storage module. Callers can load, save, or clear one
/// snapshot without taking on path, validation, or write-atomicity details.
#[derive(Clone, Debug)]
pub(crate) struct LocalCalendarStore {
    path: PathBuf,
}

impl LocalCalendarStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "calendar-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent file is a normal first launch. Invalid storage is reported so
    /// the caller can recover without replacing unknown data.
    pub(crate) fn load(&self) -> Result<Option<StoredCalendar>, CalendarStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => CalendarStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => CalendarStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, migrated) = decode_snapshot(&self.path, &bytes)?;
        if migrated {
            // Replacing the complete v1 snapshot only after successful
            // validation and encoding prevents a prior version from silently
            // reintroducing an over-midnight record.
            self.save(&snapshot)?;
        }
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the snapshot. The previous complete
    /// file remains intact if encoding or a temporary-file write fails.
    pub(crate) fn save(&self, snapshot: &StoredCalendar) -> Result<(), CalendarStoreError> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(CalendarStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(CalendarStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this local profile's calendar snapshot. Sign-out/reset
    /// flows can call this safely before the first successful save.
    pub(crate) fn clear(&self) -> Result<(), CalendarStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CalendarStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), CalendarStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| CalendarStoreError::io(self.path.clone(), error))
    }
}

impl StoredCalendar {
    /// Normalizes v1's independent hour/duration bounds into v2's explicit
    /// same-day invariant. Earlier UI creation used short workday events, but
    /// persisted v1 data could still contain a 24-hour duration, so rejecting
    /// it outright would strand a recoverable local profile.
    fn migrate(mut self) -> Result<(Self, bool), CalendarStoreError> {
        match self.schema_version {
            CALENDAR_SCHEMA_VERSION => {
                self.validate()?;
                Ok((self, false))
            }
            LEGACY_CALENDAR_SCHEMA_VERSION => {
                self.schema_version = CALENDAR_SCHEMA_VERSION;
                for event in &mut self.events {
                    if event.hour <= MAX_EVENT_HOUR {
                        event.duration_minutes = event
                            .duration_minutes
                            .min(max_duration_for_hour(event.hour));
                    }
                }
                // A weekend date cannot be represented by the workweek-only
                // UI. Unlike a duration cap, moving it to a nearby weekday
                // would silently change the event's intended date, so leave
                // the source snapshot untouched by rejecting it below.
                self.validate()?;
                Ok((self, true))
            }
            found => Err(CalendarStoreError::UnsupportedSchema { found }),
        }
    }

    fn validate(&self) -> Result<(), CalendarStoreError> {
        if self.schema_version != CALENDAR_SCHEMA_VERSION {
            return Err(CalendarStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.events.len() > MAX_EVENTS
            || self.additional_calendars.len() > MAX_ADDITIONAL_CALENDARS
        {
            return Err(CalendarStoreError::InvalidSnapshot(
                "too many stored calendar records".into(),
            ));
        }

        let mut event_ids = BTreeSet::new();
        let mut largest_event_id = 0;
        for event in &self.events {
            if event.id == 0 || !event_ids.insert(event.id) {
                return Err(CalendarStoreError::InvalidSnapshot(
                    "stored event ids must be unique and nonzero".into(),
                ));
            }
            largest_event_id = largest_event_id.max(event.id);
            event.validate()?;
        }
        if self.next_local_event_id == 0 || self.next_local_event_id <= largest_event_id {
            return Err(CalendarStoreError::InvalidSnapshot(
                "the next local event id is invalid".into(),
            ));
        }

        let mut calendar_names = BTreeSet::new();
        for calendar in &self.additional_calendars {
            if calendar.name.trim().is_empty()
                || calendar.name.len() > MAX_CALENDAR_NAME_BYTES
                || !calendar_names.insert(calendar.name.trim().to_lowercase())
            {
                return Err(CalendarStoreError::InvalidSnapshot(
                    "stored additional calendars are invalid".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Returns whether this exact snapshot can pass the local calendar store's
/// validation and encoded-size limit.
///
/// Calendar uses this before committing a prospective local event mutation.
/// That keeps accepted creates, edits, and deletes saveable even when a
/// profile is close to its total storage capacity.
pub(crate) fn snapshot_fits_local_store(snapshot: &StoredCalendar) -> bool {
    snapshot.validate().is_ok()
        && serde_json::to_vec_pretty(snapshot).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES)
}

impl StoredEvent {
    fn validate(&self) -> Result<(), CalendarStoreError> {
        if self.title.trim().is_empty()
            || self.title.len() > MAX_TITLE_BYTES
            || !is_workweek_event_date(self.year, self.month, self.day)
            || self.hour > MAX_EVENT_HOUR
            || !(MIN_DURATION_MINUTES..=max_duration_for_hour(self.hour))
                .contains(&self.duration_minutes)
        {
            return Err(CalendarStoreError::InvalidSnapshot(
                "a stored event is invalid".into(),
            ));
        }
        Ok(())
    }
}

/// Checks both Gregorian validity and whether the date is renderable by the
/// local Monday-through-Friday grid. Keep this at the persistence boundary so
/// any accepted snapshot can always be reopened through calendar search and
/// lifecycle controls.
fn is_workweek_event_date(year: i32, month: u32, day: u32) -> bool {
    if !(1..=9_999).contains(&year) {
        return false;
    }
    let (Ok(month), Ok(day)) = (u8::try_from(month), u8::try_from(day)) else {
        return false;
    };
    CalendarDate::new(year, month, day).is_some_and(CalendarDate::is_workweek_day)
}

/// Returns the longest duration that keeps an event entirely on its start
/// date. Keeping this invariant in storage, UI validation, and layout avoids
/// fabricated `24:00` timestamps and blocks overflowing into another day.
pub(crate) const fn max_duration_for_hour(hour: u8) -> u16 {
    if hour > MAX_EVENT_HOUR {
        return 0;
    }
    MAX_DURATION_MINUTES - (hour as u16) * 60
}

#[derive(Debug)]
pub(crate) enum CalendarStoreError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    Encode(serde_json::Error),
    TooLarge {
        path: PathBuf,
        bytes: u64,
    },
    UnsupportedSchema {
        found: u32,
    },
    InvalidSnapshot(String),
}

impl CalendarStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for CalendarStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local calendar storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local calendar storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local calendar storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(
                    formatter,
                    "local calendar storage is too large ({bytes} bytes)"
                )
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    formatter,
                    "local calendar storage uses unsupported version {found}"
                )
            }
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local calendar storage is invalid")
            }
        }
    }
}

impl std::error::Error for CalendarStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode(source) => Some(source),
            Self::TooLarge { .. } | Self::UnsupportedSchema { .. } | Self::InvalidSnapshot(_) => {
                None
            }
        }
    }
}

/// Decodes, migrates and validates stored bytes without touching the filesystem. The flag reports whether a migration changed the snapshot.
pub(crate) fn decode_snapshot(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<(StoredCalendar, bool), CalendarStoreError> {
    let snapshot = serde_json::from_slice::<StoredCalendar>(bytes).map_err(|source| {
        CalendarStoreError::Decode {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let (snapshot, migrated) = snapshot.migrate()?;
    Ok((snapshot, migrated))
}

/// Fuzzing seam: any bytes must decode without panicking, and a decoded
/// snapshot must re-encode to bytes that decode to the same snapshot.
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_snapshot(bytes: &[u8]) {
    let path = std::path::Path::new("fuzz.json");
    if let Ok((snapshot, _)) = decode_snapshot(path, bytes) {
        let encoded = serde_json::to_vec(&snapshot).expect("a decoded snapshot re-encodes");
        let again = decode_snapshot(path, &encoded).expect("a re-encoded snapshot decodes");
        assert!(!again.1, "a current snapshot must not migrate");
        assert_eq!(
            serde_json::to_vec(&again.0).expect("re-encodes"),
            encoded,
            "snapshot round trip is not stable"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CALENDAR_SCHEMA_VERSION, CalendarStoreError, LEGACY_CALENDAR_SCHEMA_VERSION,
        LocalCalendarStore, MAX_DURATION_MINUTES, MAX_SNAPSHOT_BYTES, MAX_TITLE_BYTES,
        StoredAdditionalCalendar, StoredCalendar, StoredEvent, max_duration_for_hour,
        snapshot_fits_local_store,
    };
    use crate::testing::TemporaryDirectory;
    use std::fs;

    fn sample_snapshot() -> StoredCalendar {
        StoredCalendar {
            schema_version: CALENDAR_SCHEMA_VERSION,
            next_local_event_id: 8,
            events: vec![StoredEvent {
                id: 7,
                title: "Design review".into(),
                year: 2026,
                month: 11,
                day: 5,
                hour: 14,
                duration_minutes: 30,
            }],
            primary_calendar_visible: true,
            calendars_open: false,
            additional_calendars: vec![StoredAdditionalCalendar {
                name: "Holidays".into(),
                visible: false,
            }],
        }
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("calendar");
        let store = LocalCalendarStore::at(directory.file("nested", "calendar-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_calendar_domain_state() {
        let directory = TemporaryDirectory::new("calendar");
        let store = LocalCalendarStore::at(directory.file("nested", "calendar-v1.json"));
        let expected = sample_snapshot();

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn v1_over_midnight_event_migrates_to_a_same_day_v2_snapshot() {
        let directory = TemporaryDirectory::new("calendar");
        let path = directory.file("nested", "calendar-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let mut legacy = sample_snapshot();
        legacy.schema_version = LEGACY_CALENDAR_SCHEMA_VERSION;
        legacy.events[0].hour = 23;
        legacy.events[0].duration_minutes = 1_440;
        fs::write(
            &path,
            serde_json::to_vec(&legacy).expect("legacy snapshot must encode"),
        )
        .expect("legacy snapshot must write");

        let store = LocalCalendarStore::at(path.clone());
        let migrated = store
            .load()
            .expect("legacy snapshot must migrate")
            .expect("legacy snapshot must exist");
        assert_eq!(migrated.schema_version, CALENDAR_SCHEMA_VERSION);
        assert_eq!(migrated.events[0].duration_minutes, 59);
        assert!(snapshot_fits_local_store(&migrated));

        let rewritten = serde_json::from_slice::<StoredCalendar>(
            &fs::read(path).expect("migrated snapshot must be rewritten atomically"),
        )
        .expect("rewritten snapshot must decode");
        assert_eq!(rewritten, migrated);
    }

    #[test]
    fn weekend_v1_event_is_rejected_without_rewriting_its_original_date() {
        let directory = TemporaryDirectory::new("calendar");
        let path = directory.file("nested", "calendar-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let mut legacy = sample_snapshot();
        legacy.schema_version = LEGACY_CALENDAR_SCHEMA_VERSION;
        // Saturday, which the local workweek grid cannot render or edit.
        legacy.events[0].day = 7;
        let encoded = serde_json::to_vec(&legacy).expect("legacy snapshot must encode");
        fs::write(&path, &encoded).expect("legacy snapshot must write");

        let store = LocalCalendarStore::at(path.clone());
        assert!(matches!(
            store.load(),
            Err(CalendarStoreError::InvalidSnapshot(_))
        ));
        assert_eq!(
            fs::read(path).expect("invalid source snapshot must remain"),
            encoded
        );
    }

    #[test]
    fn snapshot_preflight_rejects_invalid_and_oversized_calendar_data() {
        assert!(snapshot_fits_local_store(&sample_snapshot()));
        assert_eq!(max_duration_for_hour(0), MAX_DURATION_MINUTES);
        assert_eq!(max_duration_for_hour(23), 59);

        let mut invalid = sample_snapshot();
        invalid.events[0].duration_minutes = 4;
        assert!(!snapshot_fits_local_store(&invalid));

        let mut crosses_midnight = sample_snapshot();
        crosses_midnight.events[0].hour = 23;
        crosses_midnight.events[0].duration_minutes = 60;
        assert!(!snapshot_fits_local_store(&crosses_midnight));

        let mut weekend = sample_snapshot();
        weekend.events[0].day = 7;
        assert!(!snapshot_fits_local_store(&weekend));

        let mut invalid_year = sample_snapshot();
        invalid_year.events[0].year = 0;
        assert!(!snapshot_fits_local_store(&invalid_year));

        let event_count = MAX_SNAPSHOT_BYTES / MAX_TITLE_BYTES + 1;
        let oversized = StoredCalendar {
            schema_version: CALENDAR_SCHEMA_VERSION,
            next_local_event_id: event_count as u64 + 1,
            events: (1..=event_count)
                .map(|id| StoredEvent {
                    id: id as u64,
                    title: "x".repeat(MAX_TITLE_BYTES),
                    year: 2026,
                    month: 11,
                    day: 5,
                    hour: 14,
                    duration_minutes: 30,
                })
                .collect(),
            primary_calendar_visible: true,
            calendars_open: true,
            additional_calendars: Vec::new(),
        };
        assert!(!snapshot_fits_local_store(&oversized));
    }

    #[test]
    fn corrupt_unsupported_and_invalid_storage_are_rejected_without_rewriting() {
        let directory = TemporaryDirectory::new("calendar");
        let path = directory.file("nested", "calendar-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalCalendarStore::at(path.clone());

        assert!(matches!(
            store.load(),
            Err(CalendarStoreError::Decode { .. })
        ));
        assert_eq!(
            fs::read(&path).expect("corrupt file must remain"),
            b"this is not JSON"
        );

        let mut unsupported = sample_snapshot();
        unsupported.schema_version += 1;
        fs::write(
            &path,
            serde_json::to_vec(&unsupported).expect("unsupported fixture must encode"),
        )
        .expect("unsupported fixture must write");
        assert!(matches!(
            store.load(),
            Err(CalendarStoreError::UnsupportedSchema { .. })
        ));

        let mut invalid = sample_snapshot();
        invalid.events[0].day = 29;
        invalid.events[0].month = 2;
        invalid.events[0].year = 2025;
        fs::write(
            &path,
            serde_json::to_vec(&invalid).expect("invalid fixture must encode"),
        )
        .expect("invalid fixture must write");
        assert!(matches!(
            store.load(),
            Err(CalendarStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn oversized_storage_is_rejected_before_parsing() {
        let directory = TemporaryDirectory::new("calendar");
        let path = directory.file("nested", "calendar-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        let store = LocalCalendarStore::at(path);

        assert!(matches!(
            store.load(),
            Err(CalendarStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn failed_save_preserves_the_last_complete_snapshot() {
        let directory = TemporaryDirectory::new("calendar");
        let store = LocalCalendarStore::at(directory.file("nested", "calendar-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut invalid = sample_snapshot();
        invalid.schema_version = CALENDAR_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(&invalid),
            Err(CalendarStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            store.load().expect("previous snapshot must remain"),
            Some(original)
        );
    }

    #[test]
    fn replacement_leaves_one_complete_file_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("calendar");
        let store = LocalCalendarStore::at(directory.file("nested", "calendar-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut replacement = sample_snapshot();
        replacement.next_local_event_id = 9;
        store
            .save(&replacement)
            .expect("replacement snapshot must save");

        let entries = fs::read_dir(store.path().parent().expect("storage parent"))
            .expect("storage directory must be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("storage entries must be readable");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            store.load().expect("replacement snapshot must load"),
            Some(replacement)
        );

        store.clear().expect("snapshot must clear");
        store
            .clear()
            .expect("clearing a missing snapshot must be safe");
        assert_eq!(store.load().expect("cleared snapshot must load"), None);
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_are_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TemporaryDirectory::new("calendar");
        let store = LocalCalendarStore::at(directory.file("nested", "calendar-v1.json"));
        store
            .save(&sample_snapshot())
            .expect("snapshot must save securely");

        let permissions = fs::metadata(store.path())
            .expect("snapshot metadata must be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(permissions, 0o600);
    }

    #[test]
    fn decode_snapshot_is_pure_and_rejects_bad_bytes() {
        let path = std::path::Path::new("never-opened.json");
        assert!(matches!(
            super::decode_snapshot(path, b"not json"),
            Err(super::CalendarStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredCalendar::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
