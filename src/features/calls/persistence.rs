//! Durable, local-only storage for Calls-owned user preferences and history.
//!
//! The adapter deliberately excludes active-call state, media data, and
//! transient dialogs. It stores only the local profile choices that can be
//! restored safely when a future Calls adapter becomes available.

use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const CALLS_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SPEED_DIAL_ENTRIES: usize = 128;
const MAX_LOCAL_HISTORY_ENTRIES: usize = 512;
const MAX_CONTACT_BYTES: usize = 512;

/// The Calls feature's durable user-owned projection.
///
/// Active calls and call filters are intentionally absent: neither can be
/// meaningfully restored across an application restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredCalls {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) call_forwarding: StoredCallForwarding,
    #[serde(default)]
    pub(crate) audio_device: StoredAudioDevice,
    #[serde(default)]
    pub(crate) speed_dial: Vec<String>,
    #[serde(default = "default_history_open")]
    pub(crate) history_open: bool,
    #[serde(default = "default_next_local_history_id")]
    pub(crate) next_local_history_id: u64,
    #[serde(default)]
    pub(crate) local_call_history: Vec<StoredCallHistoryEntry>,
}

impl Default for StoredCalls {
    fn default() -> Self {
        Self {
            schema_version: CALLS_SCHEMA_VERSION,
            call_forwarding: StoredCallForwarding::default(),
            audio_device: StoredAudioDevice::default(),
            speed_dial: Vec::new(),
            history_open: default_history_open(),
            next_local_history_id: default_next_local_history_id(),
            local_call_history: Vec::new(),
        }
    }
}

const fn default_history_open() -> bool {
    true
}

const fn default_next_local_history_id() -> u64 {
    1
}

/// A monotonic local identifier keeps history entries stable when a later
/// adapter adds removal, synchronization, or reconciliation behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredCallHistoryEntry {
    pub(crate) id: u64,
    pub(crate) contact: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredCallForwarding {
    #[default]
    Disabled,
    Voicemail,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredAudioDevice {
    #[default]
    Internal,
    Headset,
    SystemDefault,
}

impl StoredCalls {
    fn validate(&self) -> Result<(), CallsStoreError> {
        if self.schema_version != CALLS_SCHEMA_VERSION {
            return Err(CallsStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.speed_dial.len() > MAX_SPEED_DIAL_ENTRIES
            || self.local_call_history.len() > MAX_LOCAL_HISTORY_ENTRIES
        {
            return Err(CallsStoreError::InvalidSnapshot(
                "too many stored calls records".into(),
            ));
        }

        let mut speed_dial_contacts = BTreeSet::new();
        if self.speed_dial.iter().any(|contact| {
            !valid_contact(contact) || !speed_dial_contacts.insert(contact.trim().to_lowercase())
        }) {
            return Err(CallsStoreError::InvalidSnapshot(
                "stored speed dial contacts are invalid".into(),
            ));
        }

        let mut history_ids = BTreeSet::new();
        let mut largest_history_id = 0;
        for entry in &self.local_call_history {
            if entry.id == 0 || !history_ids.insert(entry.id) || !valid_contact(&entry.contact) {
                return Err(CallsStoreError::InvalidSnapshot(
                    "stored call history is invalid".into(),
                ));
            }
            largest_history_id = largest_history_id.max(entry.id);
        }
        if self.next_local_history_id == 0 || self.next_local_history_id <= largest_history_id {
            return Err(CallsStoreError::InvalidSnapshot(
                "the next local calls history id is invalid".into(),
            ));
        }
        Ok(())
    }
}

fn valid_contact(contact: &str) -> bool {
    !contact.trim().is_empty()
        && contact.len() <= MAX_CONTACT_BYTES
        && !contact.chars().any(char::is_control)
}

/// A narrow local storage adapter for one profile's Calls projection.
#[derive(Clone, Debug)]
pub(crate) struct LocalCallsStore {
    path: PathBuf,
}

impl LocalCallsStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "calls-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent snapshot is a normal first launch. A malformed snapshot is
    /// reported rather than overwritten so a user can recover it safely.
    pub(crate) fn load(&self) -> Result<Option<StoredCalls>, CallsStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => CallsStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => CallsStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, _) = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the profile snapshot. A complete
    /// previous snapshot survives any encoding or temporary-write failure.
    pub(crate) fn save(&self, snapshot: &StoredCalls) -> Result<(), CallsStoreError> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(CallsStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(CallsStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's Calls snapshot. Sign-out/reset can invoke
    /// it before the first successful save.
    pub(crate) fn clear(&self) -> Result<(), CallsStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CallsStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), CallsStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| CallsStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum CallsStoreError {
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

impl CallsStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for CallsStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local calls storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local calls storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local calls storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(
                    formatter,
                    "local calls storage is too large ({bytes} bytes)"
                )
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    formatter,
                    "local calls storage uses unsupported version {found}"
                )
            }
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local calls storage is invalid")
            }
        }
    }
}

impl std::error::Error for CallsStoreError {
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
) -> Result<(StoredCalls, bool), CallsStoreError> {
    let snapshot =
        serde_json::from_slice::<StoredCalls>(bytes).map_err(|source| CallsStoreError::Decode {
            path: path.to_path_buf(),
            source,
        })?;
    snapshot.validate()?;
    let migrated = false;
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
        CALLS_SCHEMA_VERSION, CallsStoreError, LocalCallsStore, MAX_SNAPSHOT_BYTES,
        StoredAudioDevice, StoredCallForwarding, StoredCallHistoryEntry, StoredCalls,
    };
    use crate::testing::TemporaryDirectory;
    use std::fs;

    fn sample_snapshot() -> StoredCalls {
        StoredCalls {
            schema_version: CALLS_SCHEMA_VERSION,
            call_forwarding: StoredCallForwarding::Voicemail,
            audio_device: StoredAudioDevice::Headset,
            speed_dial: vec!["Wren Castellanos".into(), "Dara Whitlock".into()],
            history_open: false,
            next_local_history_id: 4,
            local_call_history: vec![
                StoredCallHistoryEntry {
                    id: 3,
                    contact: "Dara Whitlock".into(),
                },
                StoredCallHistoryEntry {
                    id: 2,
                    contact: "Wren Castellanos".into(),
                },
            ],
        }
    }

    #[test]
    fn default_snapshot_uses_safe_preferences_and_first_history_id() {
        let snapshot = StoredCalls::default();

        assert_eq!(snapshot.schema_version, CALLS_SCHEMA_VERSION);
        assert_eq!(snapshot.call_forwarding, StoredCallForwarding::Disabled);
        assert_eq!(snapshot.audio_device, StoredAudioDevice::Internal);
        assert!(snapshot.history_open);
        assert_eq!(snapshot.next_local_history_id, 1);
        assert!(snapshot.local_call_history.is_empty());
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("calls");
        let store = LocalCallsStore::at(directory.file("nested", "calls-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_preferences_and_stable_history_ids() {
        let directory = TemporaryDirectory::new("calls");
        let store = LocalCallsStore::at(directory.file("nested", "calls-v1.json"));
        let expected = sample_snapshot();

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn corrupt_unsupported_and_invalid_storage_are_rejected_without_rewriting() {
        let directory = TemporaryDirectory::new("calls");
        let path = directory.file("nested", "calls-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalCallsStore::at(path.clone());

        assert!(matches!(store.load(), Err(CallsStoreError::Decode { .. })));
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
            Err(CallsStoreError::UnsupportedSchema { .. })
        ));

        let mut invalid = sample_snapshot();
        invalid.next_local_history_id = 3;
        fs::write(
            &path,
            serde_json::to_vec(&invalid).expect("invalid fixture must encode"),
        )
        .expect("invalid fixture must write");
        assert!(matches!(
            store.load(),
            Err(CallsStoreError::InvalidSnapshot(_))
        ));

        let mut duplicate_speed_dial = sample_snapshot();
        duplicate_speed_dial
            .speed_dial
            .push(" dara whitlock ".into());
        fs::write(
            &path,
            serde_json::to_vec(&duplicate_speed_dial)
                .expect("duplicate speed dial fixture must encode"),
        )
        .expect("duplicate speed dial fixture must write");
        assert!(matches!(
            store.load(),
            Err(CallsStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn oversized_storage_is_rejected_before_parsing() {
        let directory = TemporaryDirectory::new("calls");
        let path = directory.file("nested", "calls-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        let store = LocalCallsStore::at(path);

        assert!(matches!(
            store.load(),
            Err(CallsStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn failed_save_preserves_the_last_complete_snapshot() {
        let directory = TemporaryDirectory::new("calls");
        let store = LocalCallsStore::at(directory.file("nested", "calls-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut invalid = sample_snapshot();
        invalid.schema_version = CALLS_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(&invalid),
            Err(CallsStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            store.load().expect("previous snapshot must remain"),
            Some(original)
        );
    }

    #[test]
    fn replacement_leaves_one_complete_file_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("calls");
        let store = LocalCallsStore::at(directory.file("nested", "calls-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut replacement = sample_snapshot();
        replacement.next_local_history_id = 5;
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

        let directory = TemporaryDirectory::new("calls");
        let store = LocalCallsStore::at(directory.file("nested", "calls-v1.json"));
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
            Err(super::CallsStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredCalls::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
