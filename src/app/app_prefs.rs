//! Durable, local-only app-wide preferences for one profile.
//!
//! Only user choices that are not about chats live here (today the colour
//! theme). Chat and notification choices stay in `chat-prefs-v1.json`, which
//! owns the mute secret. The file holds no identity, chat or message data, is
//! written privately and atomically, is bounded, and is rewritten only after a
//! successful load, so a damaged file is never silently replaced.

use crate::app::atomic_file;
use crate::ui::theme::ThemePreference;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

pub(super) const APP_PREFS_SCHEMA_VERSION: u32 = 1;
const MAX_PREFS_BYTES: usize = 8 * 1024;

/// What the signed-in user chose for the app on this device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AppPrefs {
    pub(crate) theme: ThemePreference,
}

#[derive(Deserialize, Serialize)]
struct StoredAppPrefs {
    schema_version: u32,
    #[serde(default)]
    theme: String,
}

const fn theme_key(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::Light => "light",
        ThemePreference::Dark => "dark",
        ThemePreference::System => "system",
    }
}

/// An unknown or missing value falls back to following the system.
fn theme_from_key(key: &str) -> ThemePreference {
    match key {
        "light" => ThemePreference::Light,
        "dark" => ThemePreference::Dark,
        _ => ThemePreference::System,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LocalAppPrefsStore {
    path: PathBuf,
}

impl LocalAppPrefsStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "app-prefs-v1.json").map(Self::at)
    }

    pub(crate) const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// An absent file is normal for a profile that never changed a choice.
    pub(crate) fn load(&self) -> Result<Option<AppPrefs>, AppPrefsError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_PREFS_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(_) => AppPrefsError::TooLarge,
                    None => AppPrefsError::Io,
                });
            }
        };
        let stored =
            serde_json::from_slice::<StoredAppPrefs>(&bytes).map_err(|_| AppPrefsError::Decode)?;
        // A newer schema is refused rather than half-read and overwritten.
        if stored.schema_version != APP_PREFS_SCHEMA_VERSION {
            return Err(AppPrefsError::UnsupportedSchema);
        }
        Ok(Some(AppPrefs {
            theme: theme_from_key(&stored.theme),
        }))
    }

    pub(crate) fn save(&self, prefs: AppPrefs) -> Result<(), AppPrefsError> {
        let stored = StoredAppPrefs {
            schema_version: APP_PREFS_SCHEMA_VERSION,
            theme: theme_key(prefs.theme).into(),
        };
        let bytes = serde_json::to_vec_pretty(&stored).map_err(|_| AppPrefsError::Encode)?;
        atomic_file::write_private_atomic(&self.path, &bytes).map_err(|_| AppPrefsError::Io)
    }

    /// Removes this profile's choices on sign-out or reset.
    pub(crate) fn clear(&self) -> Result<(), AppPrefsError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AppPrefsError::Io),
        }
    }
}

/// Categories only: a path or file body never reaches a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppPrefsError {
    Io,
    Decode,
    Encode,
    TooLarge,
    UnsupportedSchema,
}

impl fmt::Display for AppPrefsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "could not access local app preferences",
            Self::Decode => "could not read local app preferences",
            Self::Encode => "could not encode local app preferences",
            Self::TooLarge => "local app preferences are too large",
            Self::UnsupportedSchema => "local app preferences use an unsupported version",
        })
    }
}

impl std::error::Error for AppPrefsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TemporaryDirectory;

    fn store(directory: &TemporaryDirectory) -> LocalAppPrefsStore {
        LocalAppPrefsStore::at(directory.file("profile", "app-prefs-v1.json"))
    }

    #[test]
    fn the_default_theme_follows_the_system() {
        assert_eq!(AppPrefs::default().theme, ThemePreference::System);
    }

    #[test]
    fn a_missing_file_loads_as_none() {
        let directory = TemporaryDirectory::new("app-prefs");
        assert_eq!(store(&directory).load(), Ok(None));
    }

    #[test]
    fn every_theme_round_trips() {
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        for theme in ThemePreference::ALL {
            store.save(AppPrefs { theme }).expect("save");
            assert_eq!(store.load(), Ok(Some(AppPrefs { theme })));
        }
    }

    #[test]
    fn the_file_holds_only_the_schema_and_theme() {
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        store
            .save(AppPrefs {
                theme: ThemePreference::Dark,
            })
            .expect("save");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&store.path).expect("read")).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"schema_version":1,"theme":"dark"})
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        store.save(AppPrefs::default()).expect("save");
        let mode = std::fs::metadata(&store.path)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn an_unknown_theme_falls_back_to_system() {
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        std::fs::create_dir_all(store.path.parent().expect("parent")).expect("dir");
        std::fs::write(&store.path, r#"{"schema_version":1,"theme":"sepia"}"#).expect("write");
        assert_eq!(store.load(), Ok(Some(AppPrefs::default())));
        std::fs::write(&store.path, r#"{"schema_version":1}"#).expect("write");
        assert_eq!(store.load(), Ok(Some(AppPrefs::default())));
    }

    #[test]
    fn a_newer_or_damaged_file_is_refused() {
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        std::fs::create_dir_all(store.path.parent().expect("parent")).expect("dir");
        std::fs::write(&store.path, r#"{"schema_version":2,"theme":"dark"}"#).expect("write");
        assert_eq!(store.load(), Err(AppPrefsError::UnsupportedSchema));
        std::fs::write(&store.path, "not json").expect("write");
        assert_eq!(store.load(), Err(AppPrefsError::Decode));
        std::fs::write(&store.path, vec![b' '; MAX_PREFS_BYTES + 1]).expect("write");
        assert_eq!(store.load(), Err(AppPrefsError::TooLarge));
    }

    #[test]
    fn clear_removes_the_file_and_tolerates_absence() {
        let directory = TemporaryDirectory::new("app-prefs");
        let store = store(&directory);
        assert_eq!(store.clear(), Ok(()));
        store.save(AppPrefs::default()).expect("save");
        assert_eq!(store.clear(), Ok(()));
        assert_eq!(store.load(), Ok(None));
    }
}
