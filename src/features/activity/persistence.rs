//! Durable, local-only storage for Activity user mutations.
//!
//! The Activity feature owns the meaning of its stable item and reaction
//! keys. This module only validates and atomically stores a versioned data
//! projection; filters, selections, and drafts deliberately remain transient.

use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const ACTIVITY_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_READ_ITEM_KEYS: usize = 10_000;
const MAX_REPLY_THREADS: usize = 10_000;
const MAX_REPLIES_PER_THREAD: usize = 10_000;
const MAX_REACTION_TARGETS: usize = 10_000;
const MAX_SELECTED_REACTIONS_PER_TARGET: usize = 16;
const MAX_KEY_BYTES: usize = 512;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_REACTION_ICON_BYTES: usize = 64;

/// The durable domain projection for Activity. It records only mutations made
/// by the local user, keyed independently from source-list ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredActivity {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) read_item_keys: Vec<String>,
    #[serde(default)]
    pub(crate) unread_item_keys: Vec<String>,
    #[serde(default)]
    pub(crate) replies_by_item: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub(crate) selected_reactions_by_target: BTreeMap<String, Vec<String>>,
}

impl Default for StoredActivity {
    fn default() -> Self {
        Self {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            read_item_keys: Vec::new(),
            unread_item_keys: Vec::new(),
            replies_by_item: BTreeMap::new(),
            selected_reactions_by_target: BTreeMap::new(),
        }
    }
}

impl StoredActivity {
    fn validate(&self) -> Result<(), ActivityStoreError> {
        if self.schema_version != ACTIVITY_SCHEMA_VERSION {
            return Err(ActivityStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.read_item_keys.len() > MAX_READ_ITEM_KEYS
            || self.unread_item_keys.len() > MAX_READ_ITEM_KEYS
            || self.replies_by_item.len() > MAX_REPLY_THREADS
            || self.selected_reactions_by_target.len() > MAX_REACTION_TARGETS
        {
            return Err(ActivityStoreError::InvalidSnapshot(
                "too many stored activity records".into(),
            ));
        }

        let mut read_item_keys = BTreeSet::new();
        for key in &self.read_item_keys {
            if !valid_key(key) || !read_item_keys.insert(key) {
                return Err(ActivityStoreError::InvalidSnapshot(
                    "stored read activity keys are invalid".into(),
                ));
            }
        }

        let mut unread_item_keys = BTreeSet::new();
        for key in &self.unread_item_keys {
            if !valid_key(key) || !unread_item_keys.insert(key) || read_item_keys.contains(key) {
                return Err(ActivityStoreError::InvalidSnapshot(
                    "stored unread activity keys are invalid".into(),
                ));
            }
        }

        for (item_key, replies) in &self.replies_by_item {
            if !valid_key(item_key)
                || replies.is_empty()
                || replies.len() > MAX_REPLIES_PER_THREAD
                || replies.iter().any(|reply| !valid_reply(reply))
            {
                return Err(ActivityStoreError::InvalidSnapshot(
                    "stored activity replies are invalid".into(),
                ));
            }
        }

        for (target_key, icons) in &self.selected_reactions_by_target {
            if !valid_key(target_key)
                || icons.is_empty()
                || icons.len() > MAX_SELECTED_REACTIONS_PER_TARGET
            {
                return Err(ActivityStoreError::InvalidSnapshot(
                    "stored activity reactions are invalid".into(),
                ));
            }
            let mut seen_icons = BTreeSet::new();
            if icons
                .iter()
                .any(|icon| !valid_reaction_icon(icon) || !seen_icons.insert(icon))
            {
                return Err(ActivityStoreError::InvalidSnapshot(
                    "stored activity reactions are invalid".into(),
                ));
            }
        }
        Ok(())
    }
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= MAX_KEY_BYTES && !key.chars().any(char::is_control)
}

fn valid_reply(reply: &str) -> bool {
    !reply.trim().is_empty() && reply.len() <= MAX_REPLY_BYTES
}

fn valid_reaction_icon(icon: &str) -> bool {
    !icon.trim().is_empty()
        && icon.len() <= MAX_REACTION_ICON_BYTES
        && icon.chars().count() <= 16
        && !icon.chars().any(char::is_control)
}

/// A narrow local adapter for one profile's Activity projection.
#[derive(Clone, Debug)]
pub(crate) struct LocalActivityStore {
    path: PathBuf,
}

impl LocalActivityStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "activity-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent snapshot is an ordinary first launch. Invalid storage is
    /// reported so callers can preserve it instead of overwriting it.
    pub(crate) fn load(&self) -> Result<Option<StoredActivity>, ActivityStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => ActivityStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => ActivityStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, _) = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the profile snapshot. A prior valid
    /// snapshot remains intact whenever encoding or a temporary-file write
    /// fails before the final rename.
    pub(crate) fn save(&self, snapshot: &StoredActivity) -> Result<(), ActivityStoreError> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(ActivityStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(ActivityStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's Activity snapshot. The operation is
    /// intentionally idempotent for sign-out and reset flows.
    pub(crate) fn clear(&self) -> Result<(), ActivityStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ActivityStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), ActivityStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| ActivityStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum ActivityStoreError {
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

impl ActivityStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for ActivityStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local activity storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local activity storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local activity storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(
                    formatter,
                    "local activity storage is too large ({bytes} bytes)"
                )
            }
            Self::UnsupportedSchema { found } => write!(
                formatter,
                "local activity storage uses unsupported version {found}"
            ),
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local activity storage is invalid")
            }
        }
    }
}

impl std::error::Error for ActivityStoreError {
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
) -> Result<(StoredActivity, bool), ActivityStoreError> {
    let snapshot = serde_json::from_slice::<StoredActivity>(bytes).map_err(|source| {
        ActivityStoreError::Decode {
            path: path.to_path_buf(),
            source,
        }
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
        ACTIVITY_SCHEMA_VERSION, ActivityStoreError, LocalActivityStore, MAX_SNAPSHOT_BYTES,
        StoredActivity,
    };
    use crate::testing::TemporaryDirectory;
    use std::collections::BTreeMap;
    use std::fs;

    fn sample_snapshot() -> StoredActivity {
        StoredActivity {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            read_item_keys: vec!["parking-changes".into()],
            unread_item_keys: vec!["delivery-update".into()],
            replies_by_item: BTreeMap::from([(
                "parking-changes".into(),
                vec!["Thanks for the update.".into()],
            )]),
            selected_reactions_by_target: BTreeMap::from([
                ("parking-changes".into(), vec!["👍".into()]),
                (
                    "parking-changes:reply:gate-question".into(),
                    vec!["❤".into()],
                ),
            ]),
        }
    }

    #[test]
    fn empty_snapshot_has_the_current_schema() {
        assert_eq!(
            StoredActivity::default().schema_version,
            ACTIVITY_SCHEMA_VERSION
        );
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("activity");
        let store = LocalActivityStore::at(directory.file("nested", "activity-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_keyed_activity_mutations() {
        let directory = TemporaryDirectory::new("activity");
        let store = LocalActivityStore::at(directory.file("nested", "activity-v1.json"));
        let expected = sample_snapshot();

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn corrupt_unsupported_and_invalid_storage_are_rejected_without_rewriting() {
        let directory = TemporaryDirectory::new("activity");
        let path = directory.file("nested", "activity-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalActivityStore::at(path.clone());

        assert!(matches!(
            store.load(),
            Err(ActivityStoreError::Decode { .. })
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
            Err(ActivityStoreError::UnsupportedSchema { .. })
        ));

        let mut invalid = sample_snapshot();
        invalid.read_item_keys.push("parking-changes".into());
        fs::write(
            &path,
            serde_json::to_vec(&invalid).expect("invalid fixture must encode"),
        )
        .expect("invalid fixture must write");
        assert!(matches!(
            store.load(),
            Err(ActivityStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn oversized_storage_is_rejected_before_parsing() {
        let directory = TemporaryDirectory::new("activity");
        let path = directory.file("nested", "activity-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        let store = LocalActivityStore::at(path);

        assert!(matches!(
            store.load(),
            Err(ActivityStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn failed_save_preserves_the_last_complete_snapshot() {
        let directory = TemporaryDirectory::new("activity");
        let store = LocalActivityStore::at(directory.file("nested", "activity-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut invalid = sample_snapshot();
        invalid.schema_version = ACTIVITY_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(&invalid),
            Err(ActivityStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            store.load().expect("previous snapshot must remain"),
            Some(original)
        );
    }

    #[test]
    fn replacement_leaves_one_complete_file_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("activity");
        let store = LocalActivityStore::at(directory.file("nested", "activity-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut replacement = sample_snapshot();
        replacement.read_item_keys.push("van-booking".into());
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

        let directory = TemporaryDirectory::new("activity");
        let store = LocalActivityStore::at(directory.file("nested", "activity-v1.json"));
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
            Err(super::ActivityStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredActivity::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
