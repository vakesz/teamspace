//! Durable, local-only storage for user-added Files entries.
//!
//! The snapshot contains only canonical local paths that the user selected.
//! It never copies file contents, stores preview bytes, or records bundled
//! registry entries. On restore, paths are revalidated before they reappear
//! in the Files list; unavailable records are pruned by the normal atomic
//! local-save lifecycle without touching their source files.

use crate::app::atomic_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const FILES_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_LOCAL_UPLOADS: usize = 512;
const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;

/// The durable Files projection. Dialogs, selection, search text, current
/// view, file bytes, and bundled examples deliberately remain session-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredFiles {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) local_uploads: Vec<StoredLocalFile>,
}

impl StoredFiles {
    fn validate(&self) -> Result<(), FilesStoreError> {
        if self.schema_version != FILES_SCHEMA_VERSION {
            return Err(FilesStoreError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.local_uploads.len() > MAX_LOCAL_UPLOADS {
            return Err(FilesStoreError::InvalidSnapshot(
                "too many local file entries".into(),
            ));
        }

        let mut paths = BTreeSet::new();
        if self
            .local_uploads
            .iter()
            .any(|file| !file.is_valid() || !paths.insert(file.path.as_str()))
        {
            return Err(FilesStoreError::InvalidSnapshot(
                "stored local file entries are invalid".into(),
            ));
        }
        Ok(())
    }
}

impl Default for StoredFiles {
    fn default() -> Self {
        Self {
            schema_version: FILES_SCHEMA_VERSION,
            local_uploads: Vec::new(),
        }
    }
}

/// A portable, UTF-8-only representation of one canonical local path.
///
/// The app intentionally skips non-Unicode paths when saving: lossy path
/// conversion would make a later restore point at a different file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredLocalFile {
    pub(crate) path: String,
}

impl StoredLocalFile {
    pub(crate) fn from_local_path(path: &Path) -> Option<Self> {
        let path = path.to_str()?.to_owned();
        let stored = Self { path };
        stored.is_valid().then_some(stored)
    }

    pub(crate) fn into_path(self) -> PathBuf {
        PathBuf::from(self.path)
    }

    fn is_valid(&self) -> bool {
        !self.path.is_empty()
            && self.path.len() <= MAX_PATH_BYTES
            && !self.path.chars().any(char::is_control)
            && Path::new(&self.path).is_absolute()
    }
}

/// A narrow local storage adapter for one profile's Files projection.
#[derive(Clone, Debug)]
pub(crate) struct LocalFilesStore {
    path: PathBuf,
}

impl LocalFilesStore {
    pub(crate) fn for_profile(profile: &str) -> Option<Self> {
        crate::app::storage::profile_file(profile, "files-v1.json").map(Self::at)
    }

    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// An absent snapshot is a normal first launch. Corruption is reported
    /// without overwriting the original path list so it can be recovered.
    pub(crate) fn load(&self) -> Result<Option<StoredFiles>, FilesStoreError> {
        let bytes = match atomic_file::read_bounded(&self.path, MAX_SNAPSHOT_BYTES) {
            Ok(Some(bytes)) => bytes,
            // Absent, or removed concurrently by a reset: same as first use.
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match atomic_file::too_large_bytes(&error) {
                    Some(bytes) => FilesStoreError::TooLarge {
                        path: self.path.clone(),
                        bytes,
                    },
                    None => FilesStoreError::io(self.path.clone(), error),
                });
            }
        };

        let (snapshot, _) = decode_snapshot(&self.path, &bytes)?;
        Ok(Some(snapshot))
    }

    /// Validates and atomically replaces this profile's local file metadata.
    /// The previous complete snapshot survives encoding and temporary-write
    /// failures.
    pub(crate) fn save(&self, snapshot: &StoredFiles) -> Result<(), FilesStoreError> {
        snapshot.validate()?;
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(FilesStoreError::Encode)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(FilesStoreError::TooLarge {
                path: self.path.clone(),
                bytes: bytes.len() as u64,
            });
        }
        self.write_atomically(&bytes)
    }

    /// Removes only this profile's Files metadata. It never deletes a user
    /// file referenced by the snapshot.
    pub(crate) fn clear(&self) -> Result<(), FilesStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(FilesStoreError::io(self.path.clone(), error)),
        }
    }

    fn write_atomically(&self, bytes: &[u8]) -> Result<(), FilesStoreError> {
        atomic_file::write_private_atomic(&self.path, bytes)
            .map_err(|error| FilesStoreError::io(self.path.clone(), error))
    }
}

#[derive(Debug)]
pub(crate) enum FilesStoreError {
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

impl FilesStoreError {
    fn io(path: PathBuf, source: io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl fmt::Display for FilesStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => {
                let _ = path;
                formatter.write_str("could not access local Files storage")
            }
            Self::Decode { path, .. } => {
                let _ = path;
                formatter.write_str("local Files storage is not valid JSON")
            }
            Self::Encode(_) => formatter.write_str("could not encode local Files storage"),
            Self::TooLarge { path, bytes } => {
                let _ = path;
                write!(
                    formatter,
                    "local Files storage is too large ({bytes} bytes)"
                )
            }
            Self::UnsupportedSchema { found } => {
                write!(
                    formatter,
                    "local Files storage uses unsupported version {found}"
                )
            }
            Self::InvalidSnapshot(reason) => {
                let _ = reason;
                formatter.write_str("local Files storage is invalid")
            }
        }
    }
}

impl std::error::Error for FilesStoreError {
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
) -> Result<(StoredFiles, bool), FilesStoreError> {
    let snapshot =
        serde_json::from_slice::<StoredFiles>(bytes).map_err(|source| FilesStoreError::Decode {
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
        FILES_SCHEMA_VERSION, FilesStoreError, LocalFilesStore, MAX_LOCAL_UPLOADS,
        MAX_SNAPSHOT_BYTES, StoredFiles, StoredLocalFile,
    };
    use crate::testing::TemporaryDirectory;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn local_file(directory: &TemporaryDirectory, name: &str) -> PathBuf {
        let path = directory.path().join("uploads").join(name);
        fs::create_dir_all(path.parent().expect("upload parent")).expect("parent must exist");
        fs::write(&path, b"local preview content").expect("local upload fixture must write");
        fs::canonicalize(path).expect("fixture path must canonicalize")
    }

    fn sample_snapshot(directory: &TemporaryDirectory) -> StoredFiles {
        let path = local_file(directory, "notes.txt");
        StoredFiles {
            schema_version: FILES_SCHEMA_VERSION,
            local_uploads: vec![
                StoredLocalFile::from_local_path(&path).expect("fixture path must be persistable"),
            ],
        }
    }

    #[test]
    fn default_snapshot_is_empty_and_current() {
        let snapshot = StoredFiles::default();

        assert_eq!(snapshot.schema_version, FILES_SCHEMA_VERSION);
        assert!(snapshot.local_uploads.is_empty());
    }

    #[test]
    fn path_conversion_refuses_relative_control_and_non_unicode_paths() {
        assert!(StoredLocalFile::from_local_path(Path::new("relative.txt")).is_none());
        assert!(StoredLocalFile::from_local_path(Path::new("/tmp/line\nbreak.txt")).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            assert!(
                StoredLocalFile::from_local_path(Path::new(&std::ffi::OsString::from_vec(
                    b"/tmp/non-utf8-\xff.txt".to_vec(),
                )))
                .is_none()
            );
        }
    }

    #[test]
    fn missing_storage_is_a_first_launch_not_an_error() {
        let directory = TemporaryDirectory::new("files");
        let store = LocalFilesStore::at(directory.file("nested", "files-v1.json"));

        assert_eq!(store.load().expect("missing storage must load"), None);
    }

    #[test]
    fn round_trip_preserves_only_local_file_paths() {
        let directory = TemporaryDirectory::new("files");
        let store = LocalFilesStore::at(directory.file("nested", "files-v1.json"));
        let expected = sample_snapshot(&directory);

        store.save(&expected).expect("snapshot must save");

        assert_eq!(store.load().expect("snapshot must load"), Some(expected));
    }

    #[test]
    fn corrupt_invalid_and_oversized_storage_are_rejected_without_rewriting() {
        let directory = TemporaryDirectory::new("files");
        let path = directory.file("nested", "files-v1.json");
        fs::create_dir_all(path.parent().expect("storage parent")).expect("parent must exist");
        fs::write(&path, b"this is not JSON").expect("corrupt fixture must write");
        let store = LocalFilesStore::at(path.clone());

        assert!(matches!(store.load(), Err(FilesStoreError::Decode { .. })));
        assert_eq!(
            fs::read(&path).expect("corrupt file must remain"),
            b"this is not JSON"
        );

        let mut invalid = sample_snapshot(&directory);
        invalid.local_uploads.push(invalid.local_uploads[0].clone());
        fs::write(
            &path,
            serde_json::to_vec(&invalid).expect("invalid fixture must encode"),
        )
        .expect("invalid fixture must write");
        assert!(matches!(
            store.load(),
            Err(FilesStoreError::InvalidSnapshot(_))
        ));

        let file = fs::File::create(&path).expect("oversized fixture must create");
        file.set_len(MAX_SNAPSHOT_BYTES as u64 + 1)
            .expect("fixture length must set");
        assert!(matches!(
            store.load(),
            Err(FilesStoreError::TooLarge { .. })
        ));
    }

    #[test]
    fn snapshot_bounds_and_relative_paths_are_rejected_on_save() {
        let directory = TemporaryDirectory::new("files");
        let store = LocalFilesStore::at(directory.file("nested", "files-v1.json"));
        let invalid_path = StoredFiles {
            schema_version: FILES_SCHEMA_VERSION,
            local_uploads: vec![StoredLocalFile {
                path: "relative.txt".into(),
            }],
        };
        assert!(matches!(
            store.save(&invalid_path),
            Err(FilesStoreError::InvalidSnapshot(_))
        ));

        let bounded_path = local_file(&directory, "bounded.txt");
        let too_many = StoredFiles {
            schema_version: FILES_SCHEMA_VERSION,
            local_uploads: (0..=MAX_LOCAL_UPLOADS)
                .map(|index| StoredLocalFile {
                    path: format!("{}-{index}", bounded_path.display()),
                })
                .collect(),
        };
        assert!(matches!(
            store.save(&too_many),
            Err(FilesStoreError::InvalidSnapshot(_))
        ));
    }

    #[test]
    fn replacement_is_atomic_and_clear_never_touches_referenced_files() {
        let directory = TemporaryDirectory::new("files");
        let store = LocalFilesStore::at(directory.file("nested", "files-v1.json"));
        let original = sample_snapshot(&directory);
        let file_path = original.local_uploads[0].clone().into_path();
        store.save(&original).expect("initial snapshot must save");

        let replacement = StoredFiles::default();
        store
            .save(&replacement)
            .expect("replacement snapshot must save");
        let entries = fs::read_dir(store.path().parent().expect("storage parent"))
            .expect("storage directory must be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("storage entries must be readable");
        assert_eq!(entries.len(), 1);
        assert_eq!(store.load().expect("snapshot must load"), Some(replacement));

        store.clear().expect("snapshot must clear");
        store
            .clear()
            .expect("clearing a missing snapshot must be safe");
        assert!(file_path.exists());
        assert_eq!(store.load().expect("cleared storage must load"), None);
    }

    #[cfg(unix)]
    #[test]
    fn snapshots_are_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TemporaryDirectory::new("files");
        let store = LocalFilesStore::at(directory.file("nested", "files-v1.json"));
        store
            .save(&sample_snapshot(&directory))
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
            Err(super::FilesStoreError::Decode { .. })
        ));
        assert!(super::decode_snapshot(path, b"").is_err());

        let bytes = serde_json::to_vec(&super::StoredFiles::default()).expect("encode");
        let (decoded, migrated) = super::decode_snapshot(path, &bytes).expect("default decodes");
        assert!(!migrated);
        assert_eq!(serde_json::to_vec(&decoded).expect("encode"), bytes);

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        value["schema_version"] = serde_json::json!(u32::MAX);
        let future = serde_json::to_vec(&value).expect("encode");
        assert!(super::decode_snapshot(path, &future).is_err());
    }
}
