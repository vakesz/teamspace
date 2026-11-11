//! Durable, local-only storage for the mutable chat overlay.
//!
//! This module deliberately stores plain data-transfer objects instead of UI
//! state. The chat feature owns conversions to and from its egui models;
//! callers only need to load, save, or clear one versioned snapshot.

use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Version 4 adds the Sad and Angry stored reaction kinds. Their JSON names
/// would not decode in an older build, so the bump makes an older writer
/// refuse the file instead of failing or dropping those reactions on a later
/// save. Version 3 data needs no conversion.
pub(crate) const CHAT_SCHEMA_VERSION: u32 = 4;
/// Version 3 added durable stable-ID pins for local conversations.
const LOCAL_PINS_SCHEMA_VERSION: u32 = 3;
/// Version 2 introduced explicit local-message identities.
const LOCAL_MESSAGE_IDENTITIES_SCHEMA_VERSION: u32 = 2;
const LEGACY_CHAT_SCHEMA_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONVERSATIONS: usize = 10_000;
const MAX_MESSAGES_PER_CONVERSATION: usize = 10_000;
/// Bound durable local-chat labels before they reach the egui sidebar.
///
/// Keep this next to snapshot validation rather than only at the input field:
/// a damaged or manually edited profile must not restore a value the local
/// lifecycle itself would refuse to create.
pub(crate) const MAX_LOCAL_CONVERSATION_NAME_BYTES: usize = 512;
/// Initials are derived from a local-chat name at runtime, but persisted
/// snapshots are untrusted input. A separate bound prevents a malformed cache
/// from rendering an arbitrarily large avatar label. It leaves ample space for
/// Unicode uppercase expansion of the two derived initials.
pub(crate) const MAX_LOCAL_CONVERSATION_INITIALS_BYTES: usize = 64;
pub(crate) const MAX_MESSAGE_TEXT_BYTES: usize = 64 * 1024;

/// The persisted portion of chat state. It intentionally excludes selection,
/// popovers, tabs, focus, and other transient presentation state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredChat {
    pub(crate) schema_version: u32,
    pub(crate) next_local_conversation_id: u64,
    /// Monotonic identity source for locally authored messages. During the
    /// explicit v1-to-v2 migration, old messages deliberately remain
    /// reference-only rather than inferring authorship from display text or
    /// timestamps.
    #[serde(default = "default_next_local_message_id")]
    pub(crate) next_local_message_id: u64,
    #[serde(default)]
    pub(crate) conversations: BTreeMap<String, StoredConversation>,
    #[serde(default)]
    pub(crate) local_conversations: Vec<StoredLocalConversation>,
    /// Ordered local-only pins. Source Favorites are fixture-owned and never
    /// appear here; IDs must reference durable local conversations.
    #[serde(default)]
    pub(crate) pinned_local_conversation_ids: Vec<u64>,
}

impl Default for StoredChat {
    fn default() -> Self {
        Self {
            schema_version: CHAT_SCHEMA_VERSION,
            next_local_conversation_id: 1,
            next_local_message_id: 1,
            conversations: BTreeMap::new(),
            local_conversations: Vec::new(),
            pinned_local_conversation_ids: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredConversation {
    #[serde(default)]
    pub(crate) messages: Vec<StoredMessage>,
    #[serde(default)]
    pub(crate) notes: String,
    #[serde(default)]
    pub(crate) draft: String,
    #[serde(default)]
    pub(crate) pending_attachment: Option<StoredAttachment>,
    #[serde(default)]
    pub(crate) read: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredLocalConversation {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) initials: String,
    pub(crate) color_rgb: [u8; 3],
    pub(crate) state: StoredConversation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredMessage {
    /// Stable identity exists only for messages authored by this local
    /// projection. Fixture and legacy messages intentionally have no ID and
    /// cannot be edited or deleted by the local lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local_id: Option<u64>,
    pub(crate) name: String,
    pub(crate) time: String,
    pub(crate) text: String,
    pub(crate) color_rgb: [u8; 3],
    pub(crate) own: bool,
    #[serde(default)]
    pub(crate) attachment: Option<StoredAttachment>,
    #[serde(default)]
    pub(crate) day_divider_before: bool,
    #[serde(default)]
    pub(crate) reply_context: Option<StoredReply>,
    #[serde(default)]
    pub(crate) reactions: Vec<StoredReaction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredReply {
    pub(crate) name: String,
    pub(crate) time: String,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StoredAttachment {
    #[serde(alias = "model-settings")]
    LayoutMockup,
    Spreadsheet,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredReaction {
    pub(crate) kind: StoredReactionKind,
    pub(crate) count: usize,
    #[serde(default)]
    pub(crate) reacted_by_current_user: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StoredReactionKind {
    Like,
    Heart,
    Laugh,
    Surprised,
    Sad,
    Angry,
}

/// A concrete local storage module. Its interface is deliberately narrow so
/// callers cannot couple themselves to file naming, validation, or atomic
/// write mechanics.
#[derive(Clone, Debug)]
pub(crate) struct LocalChatStore {
    path: PathBuf,
}

impl LocalChatStore {
    /// Builds a profile-scoped store outside the repository. The profile name
    /// is represented by a stable opaque identifier so account-like labels do
    /// not become filenames. The historical filename remains stable so older
    /// snapshots can be migrated atomically into the current schema in place.
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "chat-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Returns `Ok(None)` for a first launch. Corrupt, incompatible, or
    /// oversized storage is reported so the caller can preserve it for
    /// recovery instead of silently overwriting it.
    pub(crate) fn load(&self) -> Result<Option<StoredChat>, ChatStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => ChatStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => ChatStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, migrated) = decode_snapshot(&self.path, &bytes)?;
        if migrated {
            // Persist the schema boundary immediately. A v1 binary will now
            // reject this file instead of treating the new identity fields as
            // unknown JSON and silently stripping them on a later save.
            self.save(&snapshot)?;
        }
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces the snapshot. The prior complete file
    /// remains intact whenever encoding or the temporary-file write fails
    /// before the final rename.
    pub(crate) fn save(&self, snapshot: &StoredChat) -> Result<(), ChatStoreError> {
        let bytes = snapshot.encoded_for_persistence(self.path.clone())?;
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's chat snapshot. It is idempotent so sign-out
    /// and reset flows can call it safely before a first save.
    pub(crate) fn clear(&self) -> Result<(), ChatStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ChatStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), ChatStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| ChatStoreError::io(self.path.clone(), error))
    }
}

impl StoredChat {
    /// Converts supported historical snapshot shapes to the current schema.
    /// V1 had no durable message identity contract, so all of its messages
    /// remain immutable reference content after migration. V2 had no local
    /// pin contract, so it migrates with an empty pin list. V3 migrates
    /// unchanged apart from its version.
    fn migrate(mut self) -> Result<(Self, bool), ChatStoreError> {
        match self.schema_version {
            CHAT_SCHEMA_VERSION => {
                self.validate()?;
                Ok((self, false))
            }
            LOCAL_PINS_SCHEMA_VERSION => {
                // V3 differs only in the reaction kinds it may contain, which
                // are a strict subset of v4. Keep its pins and rewrite it.
                self.schema_version = CHAT_SCHEMA_VERSION;
                self.validate()?;
                Ok((self, true))
            }
            LOCAL_MESSAGE_IDENTITIES_SCHEMA_VERSION => {
                self.schema_version = CHAT_SCHEMA_VERSION;
                // The v2 shape never owned durable local pins. Discard an
                // injected field rather than treating it as a v3 contract.
                self.pinned_local_conversation_ids.clear();
                self.validate()?;
                Ok((self, true))
            }
            LEGACY_CHAT_SCHEMA_VERSION => {
                self.schema_version = CHAT_SCHEMA_VERSION;
                self.next_local_message_id = 1;
                self.pinned_local_conversation_ids.clear();
                for conversation in self.conversations.values_mut() {
                    clear_legacy_message_ids(conversation);
                }
                for conversation in &mut self.local_conversations {
                    clear_legacy_message_ids(&mut conversation.state);
                }
                self.validate()?;
                Ok((self, true))
            }
            found => Err(ChatStoreError::UnsupportedSchema { found }),
        }
    }

    /// Performs the exact validation and encoded-size check used by the
    /// atomic store. Feature code uses this before accepting an edit or
    /// deletion, so a UI-visible mutation is always representable by its
    /// durable local projection.
    pub(crate) fn validate_for_persistence(&self) -> Result<(), ChatStoreError> {
        let _ = self.encoded_for_persistence(PathBuf::new())?;
        Ok(())
    }

    fn encoded_for_persistence(&self, path: PathBuf) -> Result<Vec<u8>, ChatStoreError> {
        self.validate()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(ChatStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(ChatStoreError::TooLarge {
                path,
                bytes: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), ChatStoreError> {
        if self.schema_version != CHAT_SCHEMA_VERSION {
            return Err(ChatStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.conversations.len() > MAX_CONVERSATIONS
            || self.local_conversations.len() > MAX_CONVERSATIONS
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "too many stored conversations".into(),
            ));
        }
        let mut local_message_ids = BTreeSet::new();
        let mut largest_local_message_id = 0;
        for (id, conversation) in &self.conversations {
            if id.trim().is_empty() {
                return Err(ChatStoreError::InvalidSnapshot(
                    "a stored conversation id is empty".into(),
                ));
            }
            conversation.validate()?;
            collect_local_message_ids(
                conversation,
                &mut local_message_ids,
                &mut largest_local_message_id,
            )?;
        }

        let mut local_ids = BTreeSet::new();
        let mut largest_local_id = 0;
        for conversation in &self.local_conversations {
            if conversation.id == 0
                || !local_ids.insert(conversation.id)
                || conversation.name.trim().is_empty()
                || conversation.name.len() > MAX_LOCAL_CONVERSATION_NAME_BYTES
                || conversation.initials.trim().is_empty()
                || conversation.initials.len() > MAX_LOCAL_CONVERSATION_INITIALS_BYTES
            {
                return Err(ChatStoreError::InvalidSnapshot(
                    "a stored local conversation is invalid".into(),
                ));
            }
            largest_local_id = largest_local_id.max(conversation.id);
            conversation.state.validate()?;
            collect_local_message_ids(
                &conversation.state,
                &mut local_message_ids,
                &mut largest_local_message_id,
            )?;
        }
        if self.pinned_local_conversation_ids.len() > MAX_CONVERSATIONS {
            return Err(ChatStoreError::InvalidSnapshot(
                "too many stored local conversation pins".into(),
            ));
        }
        let mut pinned_local_ids = BTreeSet::new();
        if self
            .pinned_local_conversation_ids
            .iter()
            .any(|id| *id == 0 || !local_ids.contains(id) || !pinned_local_ids.insert(*id))
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "stored local conversation pins are invalid".into(),
            ));
        }
        if self.next_local_conversation_id == 0
            || self.next_local_conversation_id <= largest_local_id
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "the next local conversation id is invalid".into(),
            ));
        }
        if self.next_local_message_id == 0 || self.next_local_message_id <= largest_local_message_id
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "the next local message id is invalid".into(),
            ));
        }
        Ok(())
    }
}

fn clear_legacy_message_ids(conversation: &mut StoredConversation) {
    for message in &mut conversation.messages {
        message.local_id = None;
    }
}

fn default_next_local_message_id() -> u64 {
    1
}

fn collect_local_message_ids(
    conversation: &StoredConversation,
    local_message_ids: &mut BTreeSet<u64>,
    largest_local_message_id: &mut u64,
) -> Result<(), ChatStoreError> {
    for message in &conversation.messages {
        let Some(local_id) = message.local_id else {
            continue;
        };
        if local_id == 0 || !message.own || !local_message_ids.insert(local_id) {
            return Err(ChatStoreError::InvalidSnapshot(
                "stored local message identities are invalid".into(),
            ));
        }
        *largest_local_message_id = (*largest_local_message_id).max(local_id);
    }
    Ok(())
}

impl StoredConversation {
    fn validate(&self) -> Result<(), ChatStoreError> {
        if self.messages.len() > MAX_MESSAGES_PER_CONVERSATION
            || self.notes.len() > MAX_MESSAGE_TEXT_BYTES
            || self.draft.len() > MAX_MESSAGE_TEXT_BYTES
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "stored conversation content is too large".into(),
            ));
        }
        for message in &self.messages {
            message.validate()?;
        }
        Ok(())
    }
}

impl StoredMessage {
    fn validate(&self) -> Result<(), ChatStoreError> {
        if self.name.trim().is_empty()
            || self.time.trim().is_empty()
            || self.text.len() > MAX_MESSAGE_TEXT_BYTES
            || self.reply_context.as_ref().is_some_and(|reply| {
                reply.name.trim().is_empty()
                    || reply.time.trim().is_empty()
                    || reply.text.len() > MAX_MESSAGE_TEXT_BYTES
            })
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "a stored message is invalid".into(),
            ));
        }

        let mut reaction_kinds = BTreeSet::new();
        if self
            .reactions
            .iter()
            .any(|reaction| reaction.count == 0 || !reaction_kinds.insert(reaction.kind))
        {
            return Err(ChatStoreError::InvalidSnapshot(
                "stored message reactions are invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum ChatStoreError {
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

impl ChatStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for ChatStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local chat storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local chat storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local chat storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(formatter, "local chat storage is too large ({bytes} bytes)")
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    formatter,
                    "local chat storage uses unsupported version {found}"
                )
            }
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local chat storage is invalid")
            }
        }
    }
}

impl std::error::Error for ChatStoreError {
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
) -> Result<(StoredChat, bool), ChatStoreError> {
    let snapshot =
        serde_json::from_slice::<StoredChat>(bytes).map_err(|source| ChatStoreError::Decode {
            path: path.to_path_buf(),
            source,
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
    #[test]
    fn the_legacy_attachment_token_still_deserializes() {
        let legacy: super::StoredAttachment =
            serde_json::from_str("\"model-settings\"").expect("legacy token must load");
        assert_eq!(legacy, super::StoredAttachment::LayoutMockup);
        assert_eq!(
            serde_json::to_string(&legacy).expect("token must serialize"),
            "\"layout-mockup\""
        );
    }

    use super::{
        CHAT_SCHEMA_VERSION, ChatStoreError, LEGACY_CHAT_SCHEMA_VERSION,
        LOCAL_MESSAGE_IDENTITIES_SCHEMA_VERSION, LocalChatStore,
        MAX_LOCAL_CONVERSATION_INITIALS_BYTES, MAX_LOCAL_CONVERSATION_NAME_BYTES,
        MAX_SNAPSHOT_BYTES, StoredAttachment, StoredChat, StoredConversation,
        StoredLocalConversation, StoredMessage, StoredReaction, StoredReactionKind, StoredReply,
    };
    use crate::testing::TemporaryDirectory;
    use std::collections::BTreeMap;
    use std::fs;

    fn sample_snapshot() -> StoredChat {
        let mut conversations = BTreeMap::new();
        conversations.insert(
            "workspace-design".into(),
            StoredConversation {
                messages: vec![StoredMessage {
                    local_id: Some(3),
                    name: "You".into(),
                    time: "Now".into(),
                    text: "Ready for review".into(),
                    color_rgb: [91, 66, 196],
                    own: true,
                    attachment: Some(StoredAttachment::Spreadsheet),
                    day_divider_before: false,
                    reply_context: Some(StoredReply {
                        name: "Avery".into(),
                        time: "09:42".into(),
                        text: "Please share the spreadsheet".into(),
                    }),
                    reactions: vec![StoredReaction {
                        kind: StoredReactionKind::Heart,
                        count: 2,
                        reacted_by_current_user: true,
                    }],
                }],
                notes: "Review the open questions".into(),
                draft: "A saved draft".into(),
                pending_attachment: Some(StoredAttachment::LayoutMockup),
                read: true,
            },
        );
        StoredChat {
            schema_version: CHAT_SCHEMA_VERSION,
            next_local_conversation_id: 8,
            next_local_message_id: 4,
            conversations,
            local_conversations: vec![StoredLocalConversation {
                id: 7,
                name: "Design review".into(),
                initials: "DR".into(),
                color_rgb: [120, 130, 201],
                state: StoredConversation::default(),
            }],
            pinned_local_conversation_ids: vec![7],
        }
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_the_full_chat_snapshot() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let expected = sample_snapshot();

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn corrupt_and_unsupported_snapshots_are_rejected_without_rewriting_them() {
        let directory = TemporaryDirectory::new("chat");
        let path = directory.file("nested", "chat-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalChatStore::at(path.clone());

        assert!(matches!(store.load(), Err(ChatStoreError::Decode { .. })));
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
            Err(ChatStoreError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn oversized_storage_is_rejected_before_parsing() {
        let directory = TemporaryDirectory::new("chat");
        let path = directory.file("nested", "chat-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        let store = LocalChatStore::at(path);

        assert!(matches!(store.load(), Err(ChatStoreError::TooLarge { .. })));
    }

    #[test]
    fn failed_save_preserves_the_last_complete_snapshot() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut invalid = sample_snapshot();
        invalid.schema_version = CHAT_SCHEMA_VERSION + 1;
        assert!(matches!(
            store.save(&invalid),
            Err(ChatStoreError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            store.load().expect("previous snapshot must remain"),
            Some(original)
        );
    }

    #[test]
    fn oversized_local_conversation_identity_is_rejected_without_rewriting_storage() {
        let directory = TemporaryDirectory::new("chat");
        let path = directory.file("nested", "chat-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let store = LocalChatStore::at(path.clone());

        let mut oversized_name = sample_snapshot();
        oversized_name.local_conversations[0].name =
            "n".repeat(MAX_LOCAL_CONVERSATION_NAME_BYTES + 1);
        let oversized_name_bytes =
            serde_json::to_vec(&oversized_name).expect("oversized-name snapshot must encode");
        fs::write(&path, &oversized_name_bytes).expect("oversized-name fixture must write");
        assert!(matches!(
            store.load(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));
        assert_eq!(
            fs::read(&path).expect("invalid snapshot must remain for recovery"),
            oversized_name_bytes
        );

        let mut oversized_initials = sample_snapshot();
        oversized_initials.local_conversations[0].initials =
            "I".repeat(MAX_LOCAL_CONVERSATION_INITIALS_BYTES + 1);
        assert!(matches!(
            oversized_initials.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn local_message_identity_validation_rejects_zero_duplicate_and_nonmonotonic_ids() {
        let mut zero = sample_snapshot();
        zero.conversations
            .get_mut("workspace-design")
            .expect("sample conversation")
            .messages[0]
            .local_id = Some(0);
        assert!(matches!(
            zero.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));

        let mut duplicate = sample_snapshot();
        let message = duplicate
            .conversations
            .get("workspace-design")
            .expect("sample conversation")
            .messages[0]
            .clone();
        duplicate
            .conversations
            .get_mut("workspace-design")
            .expect("sample conversation")
            .messages
            .push(message);
        assert!(matches!(
            duplicate.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));

        let mut nonmonotonic = sample_snapshot();
        nonmonotonic.next_local_message_id = 3;
        assert!(matches!(
            nonmonotonic.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn local_conversation_pin_validation_rejects_zero_duplicate_and_unknown_ids() {
        let mut zero = sample_snapshot();
        zero.pinned_local_conversation_ids = vec![0];
        assert!(matches!(
            zero.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));

        let mut duplicate = sample_snapshot();
        duplicate.pinned_local_conversation_ids = vec![7, 7];
        assert!(matches!(
            duplicate.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));

        let mut unknown = sample_snapshot();
        unknown.pinned_local_conversation_ids = vec![8];
        assert!(matches!(
            unknown.validate_for_persistence(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));

        let directory = TemporaryDirectory::new("chat");
        let path = directory.file("nested", "chat-v1.json");
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let encoded = serde_json::to_vec(&unknown).expect("invalid snapshot must encode");
        fs::write(&path, &encoded).expect("invalid snapshot must write");
        let store = LocalChatStore::at(path.clone());
        assert!(matches!(
            store.load(),
            Err(ChatStoreError::InvalidSnapshot(_))
        ));
        assert_eq!(
            fs::read(path).expect("invalid snapshot must remain for recovery"),
            encoded
        );
    }

    #[test]
    fn legacy_messages_without_local_ids_remain_valid_reference_content() {
        let mut legacy = sample_snapshot();
        legacy
            .conversations
            .get_mut("workspace-design")
            .expect("sample conversation")
            .messages[0]
            .local_id = None;
        legacy.next_local_message_id = 1;

        assert!(legacy.validate_for_persistence().is_ok());
    }

    #[test]
    fn v1_json_migrates_to_v3_as_immutable_reference_content_without_pins() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let mut encoded = serde_json::to_value(sample_snapshot()).expect("sample must encode");
        encoded["schema_version"] = serde_json::json!(LEGACY_CHAT_SCHEMA_VERSION);
        encoded
            .as_object_mut()
            .expect("snapshot must encode as an object")
            .remove("next_local_message_id");
        encoded
            .as_object_mut()
            .expect("snapshot must encode as an object")
            .remove("pinned_local_conversation_ids");
        encoded["conversations"]["workspace-design"]["messages"][0]
            .as_object_mut()
            .expect("message must encode as an object")
            .remove("local_id");

        fs::write(
            &path,
            serde_json::to_vec(&encoded).expect("legacy snapshot must encode"),
        )
        .expect("legacy snapshot must write");
        let migrated = store
            .load()
            .expect("legacy snapshot must migrate")
            .expect("legacy snapshot must exist");
        assert_eq!(migrated.schema_version, CHAT_SCHEMA_VERSION);
        assert_eq!(migrated.next_local_message_id, 1);
        assert!(migrated.pinned_local_conversation_ids.is_empty());
        assert_eq!(
            migrated.conversations["workspace-design"].messages[0].local_id,
            None
        );
        assert!(migrated.validate_for_persistence().is_ok());

        let rewritten: StoredChat = serde_json::from_slice(
            &fs::read(path).expect("migrated snapshot must be rewritten atomically"),
        )
        .expect("rewritten snapshot must decode");
        assert_eq!(rewritten.schema_version, CHAT_SCHEMA_VERSION);
        assert!(rewritten.pinned_local_conversation_ids.is_empty());
        assert_eq!(
            rewritten.conversations["workspace-design"].messages[0].local_id,
            None
        );
    }

    #[test]
    fn v3_json_migrates_to_v4_keeping_local_pins_and_reactions() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let original = sample_snapshot();
        let mut encoded = serde_json::to_value(&original).expect("sample must encode");
        encoded["schema_version"] = serde_json::json!(super::LOCAL_PINS_SCHEMA_VERSION);
        fs::write(
            &path,
            serde_json::to_vec(&encoded).expect("v3 snapshot must encode"),
        )
        .expect("v3 snapshot must write");

        let migrated = store
            .load()
            .expect("v3 snapshot must migrate")
            .expect("v3 snapshot must exist");

        assert_eq!(CHAT_SCHEMA_VERSION, 4);
        assert_eq!(migrated.schema_version, CHAT_SCHEMA_VERSION);
        assert_eq!(
            migrated.pinned_local_conversation_ids,
            original.pinned_local_conversation_ids
        );
        assert_eq!(
            migrated.next_local_message_id,
            original.next_local_message_id
        );
        let rewritten: StoredChat = serde_json::from_slice(
            &fs::read(path).expect("migrated snapshot must be rewritten atomically"),
        )
        .expect("rewritten snapshot must decode");
        assert_eq!(rewritten.schema_version, CHAT_SCHEMA_VERSION);
    }

    #[test]
    fn v2_json_migrates_to_v4_with_an_empty_local_pin_list() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let path = store.path().to_path_buf();
        fs::create_dir_all(path.parent().expect("nested parent")).expect("parent must exist");
        let mut encoded = serde_json::to_value(sample_snapshot()).expect("sample must encode");
        encoded["schema_version"] = serde_json::json!(LOCAL_MESSAGE_IDENTITIES_SCHEMA_VERSION);

        fs::write(
            &path,
            serde_json::to_vec(&encoded).expect("v2 snapshot must encode"),
        )
        .expect("v2 snapshot must write");
        let migrated = store
            .load()
            .expect("v2 snapshot must migrate")
            .expect("v2 snapshot must exist");

        assert_eq!(migrated.schema_version, CHAT_SCHEMA_VERSION);
        assert_eq!(migrated.next_local_message_id, 4);
        assert!(migrated.pinned_local_conversation_ids.is_empty());
        assert_eq!(
            migrated.conversations["workspace-design"].messages[0].local_id,
            Some(3)
        );

        let rewritten: StoredChat = serde_json::from_slice(
            &fs::read(path).expect("migrated snapshot must be rewritten atomically"),
        )
        .expect("rewritten snapshot must decode");
        assert_eq!(rewritten.schema_version, CHAT_SCHEMA_VERSION);
        assert!(rewritten.pinned_local_conversation_ids.is_empty());
    }

    #[test]
    fn replacement_leaves_a_single_complete_file_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
        let original = sample_snapshot();
        store.save(&original).expect("initial snapshot must save");

        let mut replacement = sample_snapshot();
        replacement.next_local_conversation_id = 9;
        store
            .save(&replacement)
            .expect("replacement snapshot must save");

        let parent = store
            .path()
            .parent()
            .expect("storage path must have a parent");
        let entries = fs::read_dir(parent)
            .expect("storage directory must be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("storage directory entries must be readable");
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

        let directory = TemporaryDirectory::new("chat");
        let store = LocalChatStore::at(directory.file("nested", "chat-v1.json"));
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
            Err(super::ChatStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredChat::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
