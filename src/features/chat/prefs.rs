//! Durable, local-only chat preferences for one profile.
//!
//! Only user choices live here (no message or chat data, nothing remote). The
//! file is written privately and atomically, is bounded, and is rewritten only
//! after a successful load, so a damaged file is never silently replaced.
//!
//! Schema 2 adds notification choices and the mute list. A muted chat is kept
//! only as a keyed, truncated digest (see `ProviderResourceRef::mute_digest`)
//! under a per-profile random secret stored in this same private file, so the
//! file never contains a chat ID and a digest cannot be matched across
//! profiles. Schema 1 files load unchanged and are upgraded on the next save.

use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const CHAT_PREFS_SCHEMA_VERSION: u32 = 2;
const MAX_PREFS_BYTES: usize = 64 * 1024;
/// Upper bound on muted chats kept per profile; the oldest is dropped first.
pub(crate) const MAX_MUTED_CHATS: usize = 500;
const SECRET_BYTES: usize = 32;
const DIGEST_BYTES: usize = 16;

/// What the signed-in user chose for chats on this device.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ChatPrefs {
    /// Tell Microsoft 365 when a chat is opened, so others see it as read.
    /// Provider-visible, so it is off until the user turns it on.
    pub(crate) send_read_receipts: bool,
    /// In-app badges and toasts for new messages. They never show a body.
    pub(crate) notifications_enabled: bool,
    /// Include the chat name in a notification. Off: a generic text.
    pub(crate) show_chat_name: bool,
    /// Also raise a system notification while the window is unfocused, when
    /// this build and machine can do so under a truthful identity. Off by
    /// default; the same count-only text, never a message body.
    pub(crate) os_notifications_enabled: bool,
    profile_secret: Option<[u8; SECRET_BYTES]>,
    /// Muted chats, oldest first.
    mute_keys: Vec<[u8; DIGEST_BYTES]>,
}

impl Default for ChatPrefs {
    fn default() -> Self {
        Self {
            send_read_receipts: false,
            notifications_enabled: true,
            show_chat_name: false,
            os_notifications_enabled: false,
            profile_secret: None,
            mute_keys: Vec::new(),
        }
    }
}

impl fmt::Debug for ChatPrefs {
    /// The secret and digests never reach a log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatPrefs")
            .field("send_read_receipts", &self.send_read_receipts)
            .field("notifications_enabled", &self.notifications_enabled)
            .field("show_chat_name", &self.show_chat_name)
            .field("os_notifications_enabled", &self.os_notifications_enabled)
            .field("muted_chats", &self.mute_keys.len())
            .finish_non_exhaustive()
    }
}

impl ChatPrefs {
    /// The per-profile secret, created on first use from the OS entropy
    /// source. `None` only if entropy is unavailable (nothing is then muted).
    pub(crate) fn secret(&mut self) -> Option<[u8; SECRET_BYTES]> {
        if self.profile_secret.is_none() {
            let mut secret = [0u8; SECRET_BYTES];
            getrandom::fill(&mut secret).ok()?;
            self.profile_secret = Some(secret);
        }
        self.profile_secret
    }

    /// The existing secret, never creating one (so a read-only check cannot
    /// dirty the file). Without a secret nothing can be muted yet.
    pub(crate) const fn existing_secret(&self) -> Option<[u8; SECRET_BYTES]> {
        self.profile_secret
    }

    pub(crate) fn is_muted(&self, digest: &[u8; DIGEST_BYTES]) -> bool {
        self.mute_keys.contains(digest)
    }

    #[cfg(test)]
    pub(crate) fn muted_count(&self) -> usize {
        self.mute_keys.len()
    }

    /// Adds or removes one digest. Returns whether anything changed. At the
    /// bound the oldest muted chat is dropped.
    pub(crate) fn set_muted(&mut self, digest: [u8; DIGEST_BYTES], muted: bool) -> bool {
        let position = self.mute_keys.iter().position(|key| *key == digest);
        match (muted, position) {
            (true, None) => {
                if self.mute_keys.len() >= MAX_MUTED_CHATS {
                    self.mute_keys.remove(0);
                }
                self.mute_keys.push(digest);
                true
            }
            (false, Some(index)) => {
                self.mute_keys.remove(index);
                true
            }
            _ => false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// The on-disk shape. Binary values are hex so the file stays plain JSON.
#[derive(Serialize, Deserialize)]
struct StoredChatPrefs {
    schema_version: u32,
    #[serde(default)]
    send_read_receipts: bool,
    #[serde(default = "default_true")]
    notifications_enabled: bool,
    #[serde(default)]
    show_chat_name: bool,
    #[serde(default)]
    os_notifications_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mute_keys: Vec<String>,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl StoredChatPrefs {
    fn from_prefs(prefs: &ChatPrefs) -> Self {
        Self {
            schema_version: CHAT_PREFS_SCHEMA_VERSION,
            send_read_receipts: prefs.send_read_receipts,
            notifications_enabled: prefs.notifications_enabled,
            show_chat_name: prefs.show_chat_name,
            os_notifications_enabled: prefs.os_notifications_enabled,
            profile_secret: prefs.profile_secret.as_ref().map(|secret| to_hex(secret)),
            mute_keys: prefs.mute_keys.iter().map(|key| to_hex(key)).collect(),
        }
    }

    fn into_prefs(self) -> Result<ChatPrefs, ChatPrefsError> {
        if self.mute_keys.len() > MAX_MUTED_CHATS {
            return Err(ChatPrefsError::Decode);
        }
        let profile_secret = match self.profile_secret {
            Some(text) => Some(from_hex::<SECRET_BYTES>(&text).ok_or(ChatPrefsError::Decode)?),
            None => None,
        };
        let mut mute_keys: Vec<[u8; DIGEST_BYTES]> = Vec::with_capacity(self.mute_keys.len());
        for text in &self.mute_keys {
            let key = from_hex::<DIGEST_BYTES>(text).ok_or(ChatPrefsError::Decode)?;
            if !mute_keys.contains(&key) {
                mute_keys.push(key);
            }
        }
        Ok(ChatPrefs {
            send_read_receipts: self.send_read_receipts,
            notifications_enabled: self.notifications_enabled,
            show_chat_name: self.show_chat_name,
            os_notifications_enabled: self.os_notifications_enabled,
            profile_secret,
            mute_keys,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LocalChatPrefsStore {
    path: PathBuf,
}

impl LocalChatPrefsStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "chat-prefs-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An absent file is normal for a profile that never changed a choice.
    pub(crate) fn load(&self) -> Result<Option<ChatPrefs>, ChatPrefsError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_PREFS_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(_) => ChatPrefsError::TooLarge,
                    None => ChatPrefsError::Io,
                });
            }
        };
        let stored = serde_json::from_slice::<StoredChatPrefs>(&bytes)
            .map_err(|_| ChatPrefsError::Decode)?;
        // Schema 1 is a subset of schema 2, so both load; a newer one is
        // refused rather than half-read and then overwritten.
        if !(1..=CHAT_PREFS_SCHEMA_VERSION).contains(&stored.schema_version) {
            return Err(ChatPrefsError::UnsupportedSchema);
        }
        stored.into_prefs().map(Some)
    }

    pub(crate) fn save(&self, prefs: &ChatPrefs) -> Result<(), ChatPrefsError> {
        let bytes = serde_json::to_vec_pretty(&StoredChatPrefs::from_prefs(prefs))
            .map_err(|_| ChatPrefsError::Encode)?;
        if bytes.len() > MAX_PREFS_BYTES {
            return Err(ChatPrefsError::TooLarge);
        }
        atomic_file::write_private_atomic(&self.path, &bytes).map_err(|_| ChatPrefsError::Io)
    }

    /// Removes this profile's choices on sign-out or reset.
    pub(crate) fn clear(&self) -> Result<(), ChatPrefsError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(ChatPrefsError::Io),
        }
    }
}

/// Categories only: a path or file body never reaches a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatPrefsError {
    Io,
    Decode,
    Encode,
    TooLarge,
    UnsupportedSchema,
}

impl fmt::Display for ChatPrefsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "could not access local chat preferences",
            Self::Decode => "could not read local chat preferences",
            Self::Encode => "could not encode local chat preferences",
            Self::TooLarge => "local chat preferences are too large",
            Self::UnsupportedSchema => "local chat preferences use an unsupported version",
        })
    }
}

impl std::error::Error for ChatPrefsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TemporaryDirectory;

    fn store(directory: &TemporaryDirectory) -> LocalChatPrefsStore {
        LocalChatPrefsStore::at(directory.file("profile", "chat-prefs-v1.json"))
    }

    fn digest(seed: u8) -> [u8; DIGEST_BYTES] {
        [seed; DIGEST_BYTES]
    }

    #[test]
    fn defaults_receipts_off_in_app_notifications_on_names_off() {
        let prefs = ChatPrefs::default();
        assert!(!prefs.send_read_receipts);
        assert!(prefs.notifications_enabled);
        assert!(!prefs.show_chat_name);
        assert!(
            !prefs.os_notifications_enabled,
            "system notifications are opt-in"
        );
        assert_eq!(prefs.muted_count(), 0);
        assert!(prefs.existing_secret().is_none());
    }

    #[test]
    fn round_trips_and_clear_is_idempotent() {
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        assert_eq!(store.load().expect("absent loads"), None);
        let mut prefs = ChatPrefs {
            send_read_receipts: true,
            notifications_enabled: false,
            show_chat_name: true,
            ..ChatPrefs::default()
        };
        assert!(prefs.secret().is_some());
        prefs.set_muted(digest(1), true);
        prefs.set_muted(digest(2), true);
        store.save(&prefs).expect("saves");
        assert_eq!(store.load().expect("loads"), Some(prefs));
        store.clear().expect("clears");
        store.clear().expect("clear is idempotent");
        assert_eq!(store.load().expect("loads"), None);
    }

    #[test]
    fn a_schema_1_file_loads_with_defaults_and_is_upgraded_on_save() {
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        fs::create_dir_all(store.path().parent().expect("parent")).expect("dir");
        fs::write(
            store.path(),
            r#"{"schema_version":1,"send_read_receipts":true}"#,
        )
        .expect("fixture");
        let prefs = store.load().expect("loads").expect("present");
        assert!(prefs.send_read_receipts);
        assert!(prefs.notifications_enabled, "new field takes its default");
        store.save(&prefs).expect("saves");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).expect("file")).expect("json");
        assert_eq!(value["schema_version"], 2);
    }

    #[test]
    fn the_file_holds_only_choices_and_opaque_digests() {
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        let mut prefs = ChatPrefs::default();
        prefs.secret();
        prefs.set_muted(digest(7), true);
        store.save(&prefs).expect("saves");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path()).expect("file")).expect("json");
        let mut keys: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "mute_keys",
                "notifications_enabled",
                "os_notifications_enabled",
                "profile_secret",
                "schema_version",
                "send_read_receipts",
                "show_chat_name"
            ]
        );
        assert_eq!(value["mute_keys"][0].as_str().expect("hex").len(), 32);
        assert_eq!(value["profile_secret"].as_str().expect("hex").len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        store.save(&ChatPrefs::default()).expect("saves");
        let mode = fs::metadata(store.path())
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_mute_list_is_bounded_deduplicated_and_drops_the_oldest() {
        let mut prefs = ChatPrefs::default();
        for index in 0..(MAX_MUTED_CHATS + 10) {
            let mut key = [0u8; DIGEST_BYTES];
            key[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert!(prefs.set_muted(key, true));
        }
        assert_eq!(prefs.muted_count(), MAX_MUTED_CHATS);
        let mut oldest = [0u8; DIGEST_BYTES];
        oldest[..8].copy_from_slice(&0u64.to_le_bytes());
        assert!(!prefs.is_muted(&oldest), "the oldest were evicted");
        assert!(!prefs.set_muted(digest(0xAA), false), "absent: no change");
        assert!(prefs.set_muted(digest(0xAA), true));
        assert!(!prefs.set_muted(digest(0xAA), true), "duplicate: no change");
        assert!(prefs.set_muted(digest(0xAA), false));
    }

    #[test]
    fn an_oversized_or_malformed_list_is_rejected_not_truncated() {
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        fs::create_dir_all(store.path().parent().expect("parent")).expect("dir");
        let many = vec![format!("{:032x}", 1); MAX_MUTED_CHATS + 1];
        let body = serde_json::json!({"schema_version":2,"mute_keys":many}).to_string();
        fs::write(store.path(), body).expect("fixture");
        assert_eq!(store.load(), Err(ChatPrefsError::Decode));
        for bad in ["zz", "abcd", &"g".repeat(32)] {
            let body = serde_json::json!({"schema_version":2,"mute_keys":[bad]}).to_string();
            fs::write(store.path(), body).expect("fixture");
            assert_eq!(store.load(), Err(ChatPrefsError::Decode), "{bad}");
        }
        let body = serde_json::json!({"schema_version":2,"profile_secret":"00"}).to_string();
        fs::write(store.path(), body).expect("fixture");
        assert_eq!(store.load(), Err(ChatPrefsError::Decode));
    }

    #[test]
    fn debug_never_prints_the_secret_or_digests() {
        let mut prefs = ChatPrefs::default();
        let secret = prefs.secret().expect("entropy");
        prefs.set_muted(digest(0xCD), true);
        let text = format!("{prefs:?}");
        assert!(!text.contains(&to_hex(&secret)));
        assert!(!text.to_lowercase().contains("cdcd"));
        assert!(text.contains("muted_chats"));
    }

    #[test]
    fn a_future_or_corrupt_file_is_rejected_and_left_untouched() {
        let directory = TemporaryDirectory::new("chat-prefs");
        let store = store(&directory);
        fs::create_dir_all(store.path().parent().expect("parent")).expect("dir");
        fs::write(
            store.path(),
            r#"{"schema_version":3,"send_read_receipts":true}"#,
        )
        .expect("fixture");
        assert_eq!(store.load(), Err(ChatPrefsError::UnsupportedSchema));
        fs::write(store.path(), "not json").expect("fixture");
        assert_eq!(store.load(), Err(ChatPrefsError::Decode));
        assert_eq!(fs::read(store.path()).expect("intact"), b"not json");
    }
}
