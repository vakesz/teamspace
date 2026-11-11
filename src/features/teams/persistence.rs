//! Durable, local-only storage for Teams-owned user mutations.
//!
//! It uses stable source keys and monotonic local-post IDs. Presentation
//! state (open composers, search, menus, and inline threads) stays transient.

use super::is_reference_community;
use crate::app::atomic_file;
use crate::data::team_catalog::channel_for_key;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const TEAMS_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHANNELS: usize = 512;
const MAX_LOCAL_POSTS: usize = 50_000;
const MAX_LOCAL_POSTS_PER_CHANNEL: usize = 10_000;
const MAX_THREAD_TARGETS_PER_CHANNEL: usize = 10_000;
const MAX_REPLIES_PER_THREAD: usize = 10_000;
const MAX_REACTION_TARGETS_PER_CHANNEL: usize = 10_000;
const MAX_SELECTED_REACTIONS_PER_TARGET: usize = 16;
const MAX_EXPANDED_TEAMS: usize = 512;
const MAX_JOINED_COMMUNITIES: usize = 512;
const MAX_CHANNEL_KEY_BYTES: usize = 512;
const MAX_SOURCE_KEY_BYTES: usize = 512;
const MAX_NAME_BYTES: usize = 512;
/// Maximum UTF-8 byte length accepted for one locally stored channel post.
///
/// Feature mutation code imports this limit so a post accepted in the UI is
/// never later rejected by the durable snapshot validator.
pub(crate) const MAX_POST_BYTES: usize = 64 * 1024;
const MAX_NOTE_BYTES: usize = 256 * 1024;
/// Maximum UTF-8 byte length accepted for one locally stored thread reply.
///
/// Keep this shared with the feature boundary for the same reason as
/// [`MAX_POST_BYTES`].
pub(crate) const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_REACTION_ICON_BYTES: usize = 64;

/// The durable domain projection for Teams. It contains only local mutations:
/// seeded catalog posts and reaction counts remain source-owned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredTeams {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) selected_channel: Option<String>,
    #[serde(default = "default_next_local_post_id")]
    pub(crate) next_local_post_id: u64,
    #[serde(default)]
    pub(crate) channels: BTreeMap<String, StoredChannelState>,
    #[serde(default = "default_teams_and_channels_open")]
    pub(crate) teams_and_channels_open: bool,
    #[serde(default = "default_communities_open")]
    pub(crate) communities_open: bool,
    #[serde(default)]
    pub(crate) expanded_teams: Vec<String>,
    #[serde(default)]
    pub(crate) joined_communities: Vec<String>,
}

impl Default for StoredTeams {
    fn default() -> Self {
        Self {
            schema_version: TEAMS_SCHEMA_VERSION,
            selected_channel: None,
            next_local_post_id: default_next_local_post_id(),
            channels: BTreeMap::new(),
            teams_and_channels_open: default_teams_and_channels_open(),
            communities_open: default_communities_open(),
            expanded_teams: Vec::new(),
            joined_communities: Vec::new(),
        }
    }
}

const fn default_next_local_post_id() -> u64 {
    1
}

const fn default_teams_and_channels_open() -> bool {
    true
}

const fn default_communities_open() -> bool {
    true
}

/// Per-channel mutable data. Source post IDs and local-post IDs are used as
/// map keys; UI positions never appear in the stored shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredChannelState {
    #[serde(default)]
    pub(crate) local_posts: Vec<StoredLocalPost>,
    #[serde(default)]
    pub(crate) notes: String,
    #[serde(default, alias = "recruitment_notes")]
    pub(crate) supplier_notes: String,
    #[serde(default)]
    pub(crate) monthly_notes: String,
    #[serde(default)]
    pub(crate) replies_by_post: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub(crate) selected_reactions_by_target: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredLocalPost {
    pub(crate) id: u64,
    pub(crate) body: String,
}

pub(crate) fn local_post_key(id: u64) -> String {
    format!("local:{id}")
}

pub(crate) fn local_post_id_from_key(key: &str) -> Option<u64> {
    key.strip_prefix("local:")
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .filter(|id| local_post_key(*id) == key)
}

impl StoredTeams {
    fn validate(&self) -> Result<(), TeamsStoreError> {
        if self.schema_version != TEAMS_SCHEMA_VERSION {
            return Err(TeamsStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.channels.len() > MAX_CHANNELS
            || self.expanded_teams.len() > MAX_EXPANDED_TEAMS
            || self.joined_communities.len() > MAX_JOINED_COMMUNITIES
        {
            return Err(TeamsStoreError::InvalidSnapshot(
                "too many stored Teams records".into(),
            ));
        }
        if self
            .selected_channel
            .as_deref()
            .is_some_and(|channel| !valid_channel_key(channel))
        {
            return Err(TeamsStoreError::InvalidSnapshot(
                "the stored selected Teams channel is invalid".into(),
            ));
        }

        let mut local_post_ids = BTreeSet::new();
        let mut largest_local_post_id = 0;
        let mut local_post_count = 0;
        for (channel_key, state) in &self.channels {
            if !valid_channel_key(channel_key) {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "a stored Teams channel key is invalid".into(),
                ));
            }
            state.validate_content(
                &mut local_post_ids,
                &mut largest_local_post_id,
                &mut local_post_count,
            )?;
        }
        if self.next_local_post_id == 0 || self.next_local_post_id <= largest_local_post_id {
            return Err(TeamsStoreError::InvalidSnapshot(
                "the next local Teams post id is invalid".into(),
            ));
        }
        for state in self.channels.values() {
            let channel_local_post_ids = state
                .local_posts
                .iter()
                .map(|post| post.id)
                .collect::<BTreeSet<_>>();
            state.validate_targets(&channel_local_post_ids)?;
        }

        validate_names(
            &self.expanded_teams,
            MAX_EXPANDED_TEAMS,
            "stored expanded Teams names are invalid",
        )?;
        validate_names(
            &self.joined_communities,
            MAX_JOINED_COMMUNITIES,
            "stored joined community names are invalid",
        )?;
        Ok(())
    }

    /// Validates a snapshot before it becomes a new on-disk representation.
    ///
    /// Loading remains deliberately more permissive for a legacy snapshot
    /// whose community list accidentally contains the source-owned reference
    /// community. `TeamsState::from_stored` removes that stale entry and marks
    /// the profile dirty so the next atomic write repairs it. New writes must
    /// never introduce it.
    fn validate_for_save(&self) -> Result<(), TeamsStoreError> {
        self.validate()?;
        if self
            .joined_communities
            .iter()
            .any(|name| is_reference_community(name))
        {
            return Err(TeamsStoreError::InvalidSnapshot(
                "the source-owned reference community cannot be stored as local membership".into(),
            ));
        }
        Ok(())
    }
}

/// Returns whether this exact snapshot can pass the local Teams store's
/// validation and encoded-size limit.
///
/// The feature uses this before committing a prospective post or reply. That
/// keeps accepted local mutations saveable even when an otherwise valid
/// profile is already close to its total storage capacity.
pub(crate) fn snapshot_fits_local_store(snapshot: &StoredTeams) -> bool {
    snapshot.validate_for_save().is_ok()
        && serde_json::to_vec_pretty(snapshot).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES)
}

impl StoredChannelState {
    fn validate_content(
        &self,
        local_post_ids: &mut BTreeSet<u64>,
        largest_local_post_id: &mut u64,
        local_post_count: &mut usize,
    ) -> Result<(), TeamsStoreError> {
        if self.local_posts.len() > MAX_LOCAL_POSTS_PER_CHANNEL
            || self.replies_by_post.len() > MAX_THREAD_TARGETS_PER_CHANNEL
            || self.selected_reactions_by_target.len() > MAX_REACTION_TARGETS_PER_CHANNEL
        {
            return Err(TeamsStoreError::InvalidSnapshot(
                "too many stored channel records".into(),
            ));
        }
        for notes in [&self.notes, &self.supplier_notes, &self.monthly_notes] {
            if notes.len() > MAX_NOTE_BYTES {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "stored Teams notes are too large".into(),
                ));
            }
        }
        for post in &self.local_posts {
            if post.id == 0
                || !local_post_ids.insert(post.id)
                || !valid_user_text(&post.body, MAX_POST_BYTES)
            {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "stored local Teams posts are invalid".into(),
                ));
            }
            *largest_local_post_id = (*largest_local_post_id).max(post.id);
            *local_post_count += 1;
            if *local_post_count > MAX_LOCAL_POSTS {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "too many stored local Teams posts".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_targets(&self, local_post_ids: &BTreeSet<u64>) -> Result<(), TeamsStoreError> {
        for (post_key, replies) in &self.replies_by_post {
            if !valid_target_key(post_key)
                || local_target_is_missing(post_key, local_post_ids)
                || replies.is_empty()
                || replies.len() > MAX_REPLIES_PER_THREAD
                || replies
                    .iter()
                    .any(|reply| !valid_user_text(reply, MAX_REPLY_BYTES))
            {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "stored Teams thread replies are invalid".into(),
                ));
            }
        }
        for (target_key, icons) in &self.selected_reactions_by_target {
            if !valid_target_key(target_key)
                || local_target_is_missing(target_key, local_post_ids)
                || icons.is_empty()
                || icons.len() > MAX_SELECTED_REACTIONS_PER_TARGET
            {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "stored Teams reactions are invalid".into(),
                ));
            }
            let mut seen_icons = BTreeSet::new();
            if icons
                .iter()
                .any(|icon| !valid_reaction_icon(icon) || !seen_icons.insert(icon))
            {
                return Err(TeamsStoreError::InvalidSnapshot(
                    "stored Teams reactions are invalid".into(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_names(
    names: &[String],
    maximum: usize,
    error_message: &str,
) -> Result<(), TeamsStoreError> {
    if names.len() > maximum {
        return Err(TeamsStoreError::InvalidSnapshot(error_message.into()));
    }
    let mut seen = BTreeSet::new();
    if names
        .iter()
        .any(|name| !valid_display_name(name) || !seen.insert(name.trim().to_lowercase()))
    {
        return Err(TeamsStoreError::InvalidSnapshot(error_message.into()));
    }
    Ok(())
}

fn valid_channel_key(key: &str) -> bool {
    valid_identifier(key, MAX_CHANNEL_KEY_BYTES) && channel_for_key(key).is_some()
}

fn valid_target_key(key: &str) -> bool {
    valid_identifier(key, MAX_SOURCE_KEY_BYTES)
        && !has_legacy_index_key(key)
        && (!key.starts_with("local:") || local_post_id_from_key(key).is_some())
}

fn local_target_is_missing(key: &str, local_post_ids: &BTreeSet<u64>) -> bool {
    local_post_id_from_key(key).is_some_and(|id| !local_post_ids.contains(&id))
}

fn has_legacy_index_key(key: &str) -> bool {
    key.rsplit_once(":reply-")
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.parse::<usize>().is_ok())
        || key
            .strip_prefix("local-post-")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.parse::<usize>().is_ok())
}

fn valid_identifier(value: &str, maximum_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum_bytes && !value.chars().any(char::is_control)
}

fn valid_display_name(value: &str) -> bool {
    valid_identifier(value, MAX_NAME_BYTES)
}

fn valid_user_text(value: &str, maximum_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum_bytes
}

fn valid_reaction_icon(icon: &str) -> bool {
    !icon.trim().is_empty()
        && icon.len() <= MAX_REACTION_ICON_BYTES
        && icon.chars().count() <= 16
        && !icon.chars().any(char::is_control)
}

/// A narrow local adapter for one profile's Teams mutation projection.
#[derive(Clone, Debug)]
pub(crate) struct LocalTeamsStore {
    path: PathBuf,
}

impl LocalTeamsStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "teams-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent snapshot is a normal first launch. Invalid storage is
    /// surfaced rather than overwritten so it remains available for recovery.
    pub(crate) fn load(&self) -> Result<Option<StoredTeams>, TeamsStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => TeamsStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => TeamsStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, _) = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the snapshot. A failed write preserves
    /// the prior complete file.
    pub(crate) fn save(&self, snapshot: &StoredTeams) -> Result<(), TeamsStoreError> {
        snapshot.validate_for_save()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(TeamsStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(TeamsStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's Teams snapshot. It is safe before the first
    /// save and can be called by sign-out/reset flows.
    pub(crate) fn clear(&self) -> Result<(), TeamsStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(TeamsStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), TeamsStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| TeamsStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum TeamsStoreError {
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

impl TeamsStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for TeamsStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local Teams storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local Teams storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local Teams storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(
                    formatter,
                    "local Teams storage is too large ({bytes} bytes)"
                )
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    formatter,
                    "local Teams storage uses unsupported version {found}"
                )
            }
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local Teams storage is invalid")
            }
        }
    }
}

impl std::error::Error for TeamsStoreError {
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
) -> Result<(StoredTeams, bool), TeamsStoreError> {
    let snapshot =
        serde_json::from_slice::<StoredTeams>(bytes).map_err(|source| TeamsStoreError::Decode {
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
    #[test]
    fn the_legacy_recruitment_notes_field_still_deserializes() {
        let legacy: super::StoredChannelState =
            serde_json::from_str(r#"{"recruitment_notes":"kept","monthly_notes":"also kept"}"#)
                .expect("legacy field must load");
        assert_eq!(legacy.supplier_notes, "kept");
        assert_eq!(legacy.monthly_notes, "also kept");
    }

    use super::{
        LocalTeamsStore, MAX_SNAPSHOT_BYTES, StoredChannelState, StoredLocalPost, StoredTeams,
        TEAMS_SCHEMA_VERSION, TeamsStoreError, local_post_id_from_key, local_post_key,
    };
    use crate::testing::TemporaryDirectory;
    use std::collections::BTreeMap;
    use std::fs;

    fn sample_snapshot() -> StoredTeams {
        StoredTeams {
            schema_version: TEAMS_SCHEMA_VERSION,
            selected_channel: Some("Bike Commuters · General".into()),
            next_local_post_id: 8,
            channels: BTreeMap::from([(
                "Bike Commuters · General".into(),
                StoredChannelState {
                    local_posts: vec![StoredLocalPost {
                        id: 7,
                        body: "A durable local update".into(),
                    }],
                    notes: "Decision: keep the rollout incremental.".into(),
                    supplier_notes: String::new(),
                    monthly_notes: String::new(),
                    replies_by_post: BTreeMap::from([(
                        local_post_key(7),
                        vec!["Thanks for the update.".into()],
                    )]),
                    selected_reactions_by_target: BTreeMap::from([
                        (local_post_key(7), vec!["👍".into()]),
                        ("locker-access:reply:access-update".into(), vec!["❤".into()]),
                    ]),
                },
            )]),
            teams_and_channels_open: true,
            communities_open: false,
            expanded_teams: vec!["Bike Commuters".into()],
            joined_communities: vec!["Book Corner".into()],
        }
    }

    #[test]
    fn default_snapshot_uses_the_current_schema_and_first_local_id() {
        let snapshot = StoredTeams::default();

        assert_eq!(snapshot.schema_version, TEAMS_SCHEMA_VERSION);
        assert_eq!(snapshot.next_local_post_id, 1);
    }

    #[test]
    fn local_post_keys_are_canonical_and_never_use_positions() {
        assert_eq!(local_post_key(42), "local:42");
        assert_eq!(local_post_id_from_key("local:42"), Some(42));
        assert_eq!(local_post_id_from_key("local:042"), None);
        assert_eq!(local_post_id_from_key("local:0"), None);
        assert_eq!(local_post_id_from_key("local-post-0"), None);
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("teams");
        let store = LocalTeamsStore::at(directory.file("nested", "teams-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_stable_keyed_teams_mutations() {
        let directory = TemporaryDirectory::new("teams");
        let store = LocalTeamsStore::at(directory.file("nested", "teams-v1.json"));
        let expected = sample_snapshot();

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn reference_community_variants_are_rejected_for_new_saves_but_load_for_repair() {
        let directory = TemporaryDirectory::new("teams");
        let path = directory.file("nested", "teams-v1.json");
        let store = LocalTeamsStore::at(path.clone());
        let mut legacy_snapshot = sample_snapshot();
        legacy_snapshot.joined_communities = vec![" whole studio community ".into()];

        assert!(matches!(
            store.save(&legacy_snapshot),
            Err(TeamsStoreError::InvalidSnapshot(_))
        ));

        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(
            &path,
            serde_json::to_vec(&legacy_snapshot).expect("legacy fixture must encode"),
        )
        .expect("legacy fixture must write");
        assert_eq!(
            store.load().expect("legacy snapshot must load for repair"),
            Some(legacy_snapshot)
        );
    }

    #[test]
    fn corrupt_unsupported_and_invalid_storage_are_rejected_without_rewriting() {
        let directory = TemporaryDirectory::new("teams");
        let path = directory.file("nested", "teams-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalTeamsStore::at(path.clone());

        assert!(matches!(store.load(), Err(TeamsStoreError::Decode { .. })));
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
            Err(TeamsStoreError::UnsupportedSchema { .. })
        ));

        let mut invalid = sample_snapshot();
        invalid.next_local_post_id = 7;
        fs::write(
            &path,
            serde_json::to_vec(&invalid).expect("invalid fixture must encode"),
        )
        .expect("invalid fixture must write");
        assert!(matches!(
            store.load(),
            Err(TeamsStoreError::InvalidSnapshot(_))
        ));

        let mut legacy_key = sample_snapshot();
        legacy_key
            .channels
            .get_mut("Bike Commuters · General")
            .expect("sample channel")
            .replies_by_post
            .insert("local-post-0".into(), vec!["unstable".into()]);
        fs::write(
            &path,
            serde_json::to_vec(&legacy_key).expect("legacy fixture must encode"),
        )
        .expect("legacy fixture must write");
        assert!(matches!(
            store.load(),
            Err(TeamsStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn oversized_storage_is_rejected_before_parsing() {
        let directory = TemporaryDirectory::new("teams");
        let path = directory.file("nested", "teams-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        let store = LocalTeamsStore::at(path);

        assert!(matches!(
            store.load(),
            Err(TeamsStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn failed_save_preserves_the_last_complete_snapshot() {
        let directory = TemporaryDirectory::new("teams");
        let store = LocalTeamsStore::at(directory.file("nested", "teams-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut invalid = sample_snapshot();
        invalid.schema_version = TEAMS_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(&invalid),
            Err(TeamsStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            store.load().expect("previous snapshot must remain"),
            Some(original)
        );
    }

    #[test]
    fn replacement_leaves_one_complete_file_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("teams");
        let store = LocalTeamsStore::at(directory.file("nested", "teams-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut replacement = sample_snapshot();
        replacement.next_local_post_id = 9;
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

        let directory = TemporaryDirectory::new("teams");
        let store = LocalTeamsStore::at(directory.file("nested", "teams-v1.json"));
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
            Err(super::TeamsStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredTeams::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
