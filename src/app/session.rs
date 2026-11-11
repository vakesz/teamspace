use crate::app::atomic_file;
use std::path::{Path, PathBuf};

const SIGNED_OUT_MARKER: &str = "teamspace:signed-out";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RestoredSession {
    pub(crate) authenticated: bool,
    pub(crate) email: String,
}

/// Tests never resolve the developer's real session file.
#[cfg(test)]
fn session_file() -> Option<PathBuf> {
    None
}

#[cfg(not(test))]
fn session_file() -> Option<PathBuf> {
    let base = crate::app::storage::config_directory_from(&|name| std::env::var_os(name))?;
    Some(base.join("teamspace").join("session"))
}

pub(crate) fn restore_session() -> RestoredSession {
    let path = session_file();
    restore_session_at(path.as_deref())
}

pub(crate) fn save_email(email: &str) -> std::io::Result<()> {
    write_session(email)
}

pub(crate) fn save_signed_out() -> std::io::Result<()> {
    write_session(SIGNED_OUT_MARKER)
}

/// Longest plausible session value: an email address plus slack.
const MAX_SESSION_BYTES: usize = 1024;

fn restore_session_at(path: Option<&Path>) -> RestoredSession {
    let Some(path) = path else {
        return RestoredSession::from_contents(None);
    };
    match read_session(path) {
        // Absent or empty is a first launch, as before.
        Ok(contents) => RestoredSession::from_contents(contents.as_deref()),
        // Unreadable or corrupt state must not silently look signed in.
        Err(()) => RestoredSession::signed_out(),
    }
}

/// `Ok(None)` is an absent or empty file; `Err` is any other failure
/// (permissions, a directory, oversize, truncated UTF-8, malformed value).
/// Failures are deliberately not logged: the value is an email address.
fn read_session(path: &Path) -> Result<Option<String>, ()> {
    let bytes = match atomic_file::read_bounded(path, MAX_SESSION_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(_) => return Err(()),
    };
    let value = String::from_utf8(bytes).map_err(|_| ())?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().any(char::is_control) {
        return Err(());
    }
    Ok(Some(value.to_owned()))
}

impl RestoredSession {
    fn signed_out() -> Self {
        Self {
            authenticated: false,
            email: String::new(),
        }
    }

    fn from_contents(contents: Option<&str>) -> Self {
        match contents {
            Some(SIGNED_OUT_MARKER) => Self::signed_out(),
            Some(email) => Self {
                authenticated: true,
                email: email.into(),
            },
            None => Self {
                // First launch remains a ready-to-use local preview.
                authenticated: true,
                email: String::new(),
            },
        }
    }
}

fn write_session(value: &str) -> std::io::Result<()> {
    let path = session_file();
    write_session_at(path.as_deref(), value)
}

fn write_session_at(path: Option<&Path>, value: &str) -> std::io::Result<()> {
    if let Some(path) = path {
        atomic_file::write_private_atomic(path, value.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RestoredSession, SIGNED_OUT_MARKER, restore_session_at, write_session_at};
    use crate::testing::TemporaryDirectory;

    #[test]
    fn persisted_session_contents_restore_the_expected_authentication_state() {
        assert_eq!(
            RestoredSession::from_contents(None),
            RestoredSession {
                authenticated: true,
                email: String::new(),
            }
        );
        assert_eq!(
            RestoredSession::from_contents(Some("alex@example.com")),
            RestoredSession {
                authenticated: true,
                email: "alex@example.com".into(),
            }
        );
        assert_eq!(
            RestoredSession::from_contents(Some(SIGNED_OUT_MARKER)),
            RestoredSession {
                authenticated: false,
                email: String::new(),
            }
        );
    }

    #[test]
    fn session_storage_creates_parent_directories_and_restores_the_latest_value() {
        let directory = TemporaryDirectory::new("session");
        let path = directory.path().join("nested").join("session");

        write_session_at(Some(&path), "alex@example.com").expect("email session must save");
        assert_eq!(
            restore_session_at(Some(&path)),
            RestoredSession {
                authenticated: true,
                email: "alex@example.com".into(),
            }
        );

        write_session_at(Some(&path), SIGNED_OUT_MARKER).expect("sign-out marker must save");
        assert_eq!(
            restore_session_at(Some(&path)),
            RestoredSession {
                authenticated: false,
                email: String::new(),
            }
        );
    }

    #[test]
    fn unreadable_or_corrupt_session_files_restore_as_signed_out() {
        let directory = TemporaryDirectory::new("session");
        let path = directory.file("", "session");
        let signed_out = RestoredSession {
            authenticated: false,
            email: String::new(),
        };

        // Truncated mid-character.
        std::fs::write(&path, &"é".as_bytes()[..1]).expect("fixture must write");
        assert_eq!(restore_session_at(Some(&path)), signed_out);
        // Binary garbage and embedded control characters.
        std::fs::write(&path, b"alex\0@example.com").expect("fixture must write");
        assert_eq!(restore_session_at(Some(&path)), signed_out);
        // Oversize.
        std::fs::write(&path, vec![b'a'; 4096]).expect("fixture must write");
        assert_eq!(restore_session_at(Some(&path)), signed_out);
        // Not a regular file.
        assert_eq!(restore_session_at(Some(directory.path())), signed_out);
    }

    #[test]
    fn absent_and_empty_session_files_keep_the_first_launch_behavior() {
        let directory = TemporaryDirectory::new("session");
        let path = directory.file("", "session");
        let first_launch = RestoredSession {
            authenticated: true,
            email: String::new(),
        };
        assert_eq!(restore_session_at(Some(&path)), first_launch);
        std::fs::write(&path, "  \n").expect("fixture must write");
        assert_eq!(restore_session_at(Some(&path)), first_launch);
    }

    #[cfg(unix)]
    #[test]
    fn session_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TemporaryDirectory::new("session");
        let path = directory.file("", "session");
        write_session_at(Some(&path), "alex@example.com").expect("session must save");
        let mode = std::fs::metadata(&path)
            .expect("metadata must read")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
