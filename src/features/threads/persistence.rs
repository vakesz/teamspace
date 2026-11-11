//! Durable, local-only storage for canonical discussion overlays.
//!
//! Reference posts and replies are not copied here. This adapter stores only
//! local replies and current-user reaction selections, so a changed catalog
//! can still provide current reference content around the durable overlay.

use super::{
    MAX_REACTION_TARGETS_PER_THREAD, MAX_REPLIES_PER_THREAD, MAX_SELECTED_REACTIONS_PER_TARGET,
    MAX_THREADS, ThreadId, ThreadTarget, valid_reaction_icon, valid_reply,
};
use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const THREADS_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// The durable projection for all shared local discussion overlays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredThreads {
    pub(super) schema_version: u32,
    #[serde(default = "default_next_local_reply_id")]
    pub(super) next_local_reply_id: u64,
    /// Set only after Activity's v1 local overlay has been copied into this
    /// canonical snapshot and this snapshot has been written atomically.
    #[serde(default)]
    pub(super) activity_v1_imported: bool,
    /// Set only after Teams' v1 local overlay has been copied into this
    /// canonical snapshot and this snapshot has been written atomically.
    #[serde(default)]
    pub(super) teams_v1_imported: bool,
    #[serde(default)]
    pub(super) threads: Vec<StoredThread>,
}

impl Default for StoredThreads {
    fn default() -> Self {
        Self {
            schema_version: THREADS_SCHEMA_VERSION,
            next_local_reply_id: default_next_local_reply_id(),
            activity_v1_imported: false,
            teams_v1_imported: false,
            threads: Vec::new(),
        }
    }
}

const fn default_next_local_reply_id() -> u64 {
    1
}

/// Stored mutation state for one exact channel post.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredThread {
    pub(super) channel: String,
    pub(super) post_key: String,
    #[serde(default)]
    pub(super) replies: Vec<StoredLocalThreadReply>,
    #[serde(default)]
    pub(super) selected_reactions: Vec<StoredReactionSelection>,
}

/// A local reply record is intentionally separate from source-owned replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredLocalThreadReply {
    pub(super) id: u64,
    pub(super) body: String,
}

/// Current-user selections for one post, source reply, or local reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredReactionSelection {
    pub(super) target: ThreadTarget,
    #[serde(default)]
    pub(super) selected_icons: Vec<String>,
}

impl StoredThreads {
    pub(super) fn validate(&self) -> Result<(), ThreadsStoreError> {
        if self.schema_version != THREADS_SCHEMA_VERSION {
            return Err(ThreadsStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.threads.len() > MAX_THREADS {
            return Err(ThreadsStoreError::InvalidSnapshot(
                "too many stored discussion threads".into(),
            ));
        }

        let mut thread_ids = BTreeSet::new();
        let mut reply_ids = BTreeSet::new();
        let mut largest_reply_id = 0;
        for thread in &self.threads {
            let id = ThreadId::new(thread.channel.clone(), thread.post_key.clone()).ok_or_else(
                || {
                    ThreadsStoreError::InvalidSnapshot(
                        "a stored discussion target is invalid".into(),
                    )
                },
            )?;
            if !thread_ids.insert(id) {
                return Err(ThreadsStoreError::InvalidSnapshot(
                    "stored discussion targets must be unique".into(),
                ));
            }
            thread.validate(&mut reply_ids, &mut largest_reply_id)?;
        }
        if self.next_local_reply_id == 0 || self.next_local_reply_id <= largest_reply_id {
            return Err(ThreadsStoreError::InvalidSnapshot(
                "the next local reply id is invalid".into(),
            ));
        }
        Ok(())
    }
}

impl StoredThread {
    fn validate(
        &self,
        all_reply_ids: &mut BTreeSet<u64>,
        largest_reply_id: &mut u64,
    ) -> Result<(), ThreadsStoreError> {
        if self.replies.len() > MAX_REPLIES_PER_THREAD
            || self.selected_reactions.len() > MAX_REACTION_TARGETS_PER_THREAD
        {
            return Err(ThreadsStoreError::InvalidSnapshot(
                "too many records in a stored discussion thread".into(),
            ));
        }
        let mut thread_reply_ids = BTreeSet::new();
        for reply in &self.replies {
            if reply.id == 0
                || !thread_reply_ids.insert(reply.id)
                || !all_reply_ids.insert(reply.id)
                || !valid_reply(&reply.body)
            {
                return Err(ThreadsStoreError::InvalidSnapshot(
                    "stored local discussion replies are invalid".into(),
                ));
            }
            *largest_reply_id = (*largest_reply_id).max(reply.id);
        }

        let mut targets = BTreeSet::new();
        for selection in &self.selected_reactions {
            if !selection.target.is_well_formed()
                || !targets.insert(selection.target.clone())
                || selection.selected_icons.is_empty()
                || selection.selected_icons.len() > MAX_SELECTED_REACTIONS_PER_TARGET
            {
                return Err(ThreadsStoreError::InvalidSnapshot(
                    "stored discussion reactions are invalid".into(),
                ));
            }
            if let ThreadTarget::LocalReply { reply_id } = &selection.target
                && !thread_reply_ids.contains(reply_id)
            {
                return Err(ThreadsStoreError::InvalidSnapshot(
                    "a stored reaction references a missing local reply".into(),
                ));
            }
            let mut icons = BTreeSet::new();
            if selection
                .selected_icons
                .iter()
                .any(|icon| !valid_reaction_icon(icon) || !icons.insert(icon))
            {
                return Err(ThreadsStoreError::InvalidSnapshot(
                    "stored discussion reactions are invalid".into(),
                ));
            }
        }
        Ok(())
    }
}

/// A narrow local adapter for one profile's canonical thread snapshot.
#[derive(Clone, Debug)]
pub(crate) struct LocalThreadStore {
    path: PathBuf,
}

impl LocalThreadStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "threads-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent snapshot is a normal first launch. Invalid data is surfaced
    /// without overwriting the original file so the user can recover it.
    pub(crate) fn load(&self) -> Result<Option<StoredThreads>, ThreadsStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => ThreadsStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => ThreadsStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, _) = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the snapshot. A complete previous
    /// snapshot survives serialization or temporary-write failures.
    pub(crate) fn save(&self, snapshot: &StoredThreads) -> Result<(), ThreadsStoreError> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(ThreadsStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(ThreadsStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes this profile's local discussion overlay only.
    pub(crate) fn clear(&self) -> Result<(), ThreadsStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ThreadsStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), ThreadsStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| ThreadsStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum ThreadsStoreError {
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

impl ThreadsStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for ThreadsStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(
                formatter,
                "could not access local discussion data at {}: {source}",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                formatter,
                "could not decode local discussion data at {}: {source}",
                path.display()
            ),
            Self::Encode(source) => write!(
                formatter,
                "could not encode local discussion data: {source}"
            ),
            Self::TooLarge { path, bytes } => write!(
                formatter,
                "local discussion data at {} is too large ({bytes} bytes)",
                path.display()
            ),
            Self::UnsupportedSchema { found } => write!(
                formatter,
                "local discussion data uses unsupported schema version {found}"
            ),
            Self::InvalidSnapshot(detail) => {
                write!(formatter, "local discussion data is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for ThreadsStoreError {
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
) -> Result<(StoredThreads, bool), ThreadsStoreError> {
    let snapshot = serde_json::from_slice::<StoredThreads>(bytes).map_err(|source| {
        ThreadsStoreError::Decode {
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
        LocalThreadStore, StoredLocalThreadReply, StoredReactionSelection, StoredThread,
        StoredThreads, THREADS_SCHEMA_VERSION, ThreadsStoreError,
    };
    use crate::features::threads::{ThreadId, ThreadStore, ThreadTarget};
    use crate::testing::TemporaryDirectory;
    use std::fs;

    fn valid_snapshot() -> StoredThreads {
        let mut store = ThreadStore::default();
        let thread = ThreadId::new("Bike Commuters · General", "route-sync")
            .expect("fixture thread must be valid");
        let reply = store
            .append_reply(thread.clone(), "A durable reply")
            .expect("fixture reply must be valid");
        store
            .toggle_reaction(
                thread,
                ThreadTarget::local_reply(reply.id()).expect("fixture target must be valid"),
                &[],
                "👍",
            )
            .expect("fixture reaction must be valid");
        store.stored()
    }

    #[test]
    fn profile_store_round_trips_a_valid_snapshot() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        let snapshot = valid_snapshot();

        store.save(&snapshot).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(snapshot));
    }

    #[test]
    fn missing_snapshot_is_an_ordinary_first_launch() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));

        assert_eq!(store.load().expect("missing snapshot must load"), None);
    }

    #[test]
    fn invalid_snapshot_does_not_replace_an_existing_valid_snapshot() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        let valid = valid_snapshot();
        store.save(&valid).expect("valid snapshot must save");
        let bytes_before = fs::read(store.path()).expect("stored snapshot must be readable");
        let invalid = StoredThreads {
            schema_version: THREADS_SCHEMA_VERSION,
            next_local_reply_id: 1,
            activity_v1_imported: false,
            teams_v1_imported: false,
            threads: vec![StoredThread {
                channel: "channel".into(),
                post_key: "post".into(),
                replies: vec![StoredLocalThreadReply {
                    id: 1,
                    body: "reply".into(),
                }],
                selected_reactions: vec![StoredReactionSelection {
                    target: ThreadTarget::local_reply(9).expect("fixture target"),
                    selected_icons: vec!["👍".into()],
                }],
            }],
        };

        assert!(matches!(
            store.save(&invalid),
            Err(ThreadsStoreError::InvalidSnapshot(_))
        ));
        assert_eq!(
            fs::read(store.path()).expect("valid snapshot must survive"),
            bytes_before
        );
    }

    #[test]
    fn malformed_and_oversize_snapshots_are_protected() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        let parent = store.path().parent().expect("profile parent");
        fs::create_dir_all(parent).expect("profile parent must be creatable");
        fs::write(store.path(), b"not json").expect("fixture must write");

        assert!(matches!(
            store.load(),
            Err(ThreadsStoreError::Decode { .. })
        ));

        fs::write(store.path(), vec![b'x'; 8 * 1024 * 1024 + 1])
            .expect("oversize fixture must write");
        assert!(matches!(
            store.load(),
            Err(ThreadsStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn clear_is_idempotent_and_removes_only_the_snapshot() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        store.save(&valid_snapshot()).expect("snapshot must save");

        store.clear().expect("clear must succeed");
        store.clear().expect("repeated clear must succeed");
        assert_eq!(store.load().expect("cleared snapshot must load"), None);
    }

    #[test]
    fn zero_local_reply_id_snapshot_is_rejected_on_load_without_panicking() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        let mut corrupt = valid_snapshot();
        corrupt.threads[0].replies[0].id = 0;
        assert!(matches!(
            store.save(&corrupt),
            Err(ThreadsStoreError::InvalidSnapshot(_))
        ));

        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("snapshot path has a parent"))
            .expect("fixture directory must be creatable");
        fs::write(
            &path,
            serde_json::to_vec(&corrupt).expect("fixture must serialize"),
        )
        .expect("fixture snapshot must be writable");
        assert!(matches!(
            store.load(),
            Err(ThreadsStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn schema_and_duplicate_targets_are_rejected() {
        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        let unsupported = StoredThreads {
            schema_version: THREADS_SCHEMA_VERSION + 1,
            ..StoredThreads::default()
        };
        assert!(matches!(
            store.save(&unsupported),
            Err(ThreadsStoreError::UnsupportedSchema { .. })
        ));

        let duplicated = StoredThreads {
            schema_version: THREADS_SCHEMA_VERSION,
            next_local_reply_id: 1,
            activity_v1_imported: false,
            teams_v1_imported: false,
            threads: vec![
                StoredThread {
                    channel: "channel".into(),
                    post_key: "post".into(),
                    replies: Vec::new(),
                    selected_reactions: Vec::new(),
                },
                StoredThread {
                    channel: "channel".into(),
                    post_key: "post".into(),
                    replies: Vec::new(),
                    selected_reactions: Vec::new(),
                },
            ],
        };
        assert!(matches!(
            store.save(&duplicated),
            Err(ThreadsStoreError::InvalidSnapshot(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_are_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TemporaryDirectory::new("threads");
        let store = LocalThreadStore::at(directory.file("profile", "threads-v1.json"));
        store.save(&valid_snapshot()).expect("snapshot must save");

        let mode = fs::metadata(store.path())
            .expect("snapshot metadata must load")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn profile_store_uses_the_canonical_threads_file_name() {
        let store = LocalThreadStore::for_profile("Alex@Example.com")
            .expect("profile path must be available");
        assert_eq!(
            store.path().file_name().and_then(|name| name.to_str()),
            Some("threads-v1.json")
        );
    }

    #[test]
    fn decode_snapshot_is_pure_and_rejects_bad_bytes() {
        let path = std::path::Path::new("never-opened.json");
        assert!(matches!(
            super::decode_snapshot(path, b"not json"),
            Err(super::ThreadsStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredThreads::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
