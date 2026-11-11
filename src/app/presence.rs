//! Local-only presence state for the account surface.
//!
//! This deliberately models how the preview is shown inside Teamspace. It is
//! not a Microsoft 365 presence adapter and never produces a provider action.

use crate::app::atomic_file;
use eframe::egui::Color32;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const PRESENCE_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024;

/// The visual presence selected for this local preview profile.
///
/// It intentionally has no "busy in a call" inference or Graph mapping: a
/// local call remains a separate preview workflow and no remote status is
/// ever changed by this enum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LocalPresence {
    #[default]
    Available,
    Away,
    DoNotDisturb,
    Offline,
}

impl LocalPresence {
    pub(crate) const ALL: [Self; 4] = [
        Self::Available,
        Self::Away,
        Self::DoNotDisturb,
        Self::Offline,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Away => "Away",
            Self::DoNotDisturb => "Do not disturb",
            Self::Offline => "Offline",
        }
    }

    /// The shared-avatar presence badge for this local status.
    pub(crate) const fn avatar_presence(self) -> crate::ui::avatar::Presence {
        use crate::ui::avatar::Presence;
        match self {
            Self::Available => Presence::Available,
            Self::Away => Presence::Away,
            Self::DoNotDisturb => Presence::Busy,
            Self::Offline => Presence::Offline,
        }
    }

    pub(crate) fn color(self) -> Color32 {
        match self {
            Self::Available => crate::ui::theme::presence_available(),
            Self::Away => crate::ui::theme::presence_away(),
            Self::DoNotDisturb => crate::ui::theme::presence_busy(),
            Self::Offline => crate::ui::theme::presence_offline(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredPresence {
    schema_version: u32,
    presence: LocalPresence,
}

impl StoredPresence {
    const fn new(presence: LocalPresence) -> Self {
        Self {
            schema_version: PRESENCE_SCHEMA_VERSION,
            presence,
        }
    }

    fn validate(self) -> Result<(), PresenceStoreError> {
        if self.schema_version != PRESENCE_SCHEMA_VERSION {
            return Err(PresenceStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        Ok(())
    }
}

/// One atomic, profile-scoped local-presence snapshot.
#[derive(Clone, Debug)]
pub(crate) struct LocalPresenceStore {
    path: PathBuf,
}

impl LocalPresenceStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "presence-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Reads one complete local-presence value. An absent file is normal for
    /// a newly signed-in local profile.
    pub(crate) fn load(&self) -> Result<Option<LocalPresence>, PresenceStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => PresenceStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => PresenceStoreError::io(self.path.clone(), error),
                });
            }
        };

        let stored = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(stored.presence))
    }

    /// Validates and atomically replaces the complete presence projection.
    pub(crate) fn save(&self, presence: LocalPresence) -> Result<(), PresenceStoreError> {
        let stored = StoredPresence::new(presence);
        stored.validate()?;
        let bytes = serde_json::to_vec_pretty(&stored).map_err(PresenceStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(PresenceStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's local-presence value during sign-out/reset.
    pub(crate) fn clear(&self) -> Result<(), PresenceStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PresenceStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), PresenceStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| PresenceStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum PresenceStoreError {
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
}

impl PresenceStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for PresenceStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                let _ = (path, source);
                formatter.write_str("could not access local presence storage")
            }
            Self::Decode { path, source } => {
                let _ = (path, source);
                formatter.write_str("could not read local presence storage")
            }
            Self::Encode(source) => {
                let _ = source;
                formatter.write_str("could not encode local presence storage")
            }
            Self::TooLarge { path, bytes } => {
                let _ = (path, bytes);
                formatter.write_str("local presence storage is too large")
            }
            Self::UnsupportedSchema { found } => {
                let _ = found;
                formatter.write_str("local presence storage uses an unsupported version")
            }
        }
    }
}

impl std::error::Error for PresenceStoreError {}

/// Decodes and validates stored presence bytes without touching the filesystem.
pub(crate) fn decode_snapshot(
    path: &std::path::Path,
    bytes: &[u8],
) -> Result<StoredPresence, PresenceStoreError> {
    let stored = serde_json::from_slice::<StoredPresence>(bytes).map_err(|source| {
        PresenceStoreError::Decode {
            path: path.to_path_buf(),
            source,
        }
    })?;
    stored.validate()?;
    Ok(stored)
}

/// Fuzzing seam: any bytes must decode without panicking, and a decoded
/// snapshot must re-encode to bytes that decode to the same snapshot.
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_snapshot(bytes: &[u8]) {
    let path = std::path::Path::new("fuzz.json");
    if let Ok(snapshot) = decode_snapshot(path, bytes) {
        let encoded = serde_json::to_vec(&snapshot).expect("a decoded snapshot re-encodes");
        let again = decode_snapshot(path, &encoded).expect("a re-encoded snapshot decodes");
        assert_eq!(
            serde_json::to_vec(&again).expect("re-encodes"),
            encoded,
            "snapshot round trip is not stable"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalPresence, LocalPresenceStore, PRESENCE_SCHEMA_VERSION, PresenceStoreError};
    use crate::testing::TemporaryDirectory;
    use std::fs;

    #[test]
    fn default_presence_is_available_and_every_status_has_a_label() {
        assert_eq!(LocalPresence::default(), LocalPresence::Available);
        assert_eq!(LocalPresence::ALL.len(), 4);
        assert!(
            LocalPresence::ALL
                .iter()
                .all(|presence| !presence.label().is_empty())
        );
    }

    #[test]
    fn presence_round_trips_atomically_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("presence");
        let store = LocalPresenceStore::at(directory.file("profile", "presence-v1.json"));

        assert_eq!(store.load().expect("new profile must load"), None);
        store
            .save(LocalPresence::DoNotDisturb)
            .expect("presence must save");
        assert_eq!(
            store.load().expect("saved profile must load"),
            Some(LocalPresence::DoNotDisturb)
        );
        assert!(store.path().is_file());

        store.clear().expect("presence must clear");
        store.clear().expect("repeated clear must be safe");
        assert_eq!(store.load().expect("cleared profile must load"), None);
    }

    #[test]
    fn corrupt_or_future_presence_snapshots_are_rejected_without_rewrite() {
        let directory = TemporaryDirectory::new("presence");
        let store = LocalPresenceStore::at(directory.file("profile", "presence-v1.json"));
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let future = format!(
            r#"{{"schema_version":{},"presence":"offline"}}"#,
            PRESENCE_SCHEMA_VERSION + 1
        );
        fs::write(&path, &future).expect("fixture must write");

        assert!(matches!(
            store.load(),
            Err(PresenceStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            fs::read_to_string(path).expect("source remains intact"),
            future
        );
    }

    #[test]
    fn separate_profile_paths_keep_local_presence_isolated() {
        let directory = TemporaryDirectory::new("presence");
        let first = LocalPresenceStore::at(directory.path().join("first").join("presence-v1.json"));
        let second =
            LocalPresenceStore::at(directory.path().join("second").join("presence-v1.json"));

        first.save(LocalPresence::Away).expect("first must save");
        second
            .save(LocalPresence::Offline)
            .expect("second must save");

        assert_eq!(
            first.load().expect("first must load"),
            Some(LocalPresence::Away)
        );
        assert_eq!(
            second.load().expect("second must load"),
            Some(LocalPresence::Offline)
        );
    }

    #[test]
    fn decode_snapshot_is_pure_and_rejects_bad_bytes() {
        let path = std::path::Path::new("never-opened.json");
        assert!(matches!(
            super::decode_snapshot(path, b"not json"),
            Err(super::PresenceStoreError::Decode { .. })
        ));
        let bytes = serde_json::to_vec(&super::StoredPresence::new(super::LocalPresence::Offline))
            .expect("encode");
        assert!(super::decode_snapshot(path, &bytes).is_ok());
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
