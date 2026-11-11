//! Shared private, atomic file I/O for profile-scoped local snapshots.
//!
//! Stores own their schemas, size limits and error enums. This module owns
//! only the crash-safe write and the bounded read that every store repeats.
//!
//! Access control: on unix files are created 0600 and directories 0700. On
//! Windows no mode bits exist; privacy comes from the ACL that `%LOCALAPPDATA%`
//! inherits from the user profile, which replaces those modes.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TEMP_FILE_ATTEMPTS: usize = 16;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const RENAME_ATTEMPTS: u32 = 5;
#[cfg(windows)]
const RENAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Carried inside an `io::Error` of kind `InvalidData` when a file exceeds the
/// caller's limit, so stores can keep reporting their own `TooLarge` variant.
#[derive(Debug)]
pub(crate) struct TooLarge {
    pub(crate) bytes: u64,
}

impl std::fmt::Display for TooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "file is too large ({} bytes)", self.bytes)
    }
}

impl std::error::Error for TooLarge {}

/// Returns the observed size when `error` came from exceeding a read limit.
pub(crate) fn too_large_bytes(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<TooLarge>())
        .map(|too_large| too_large.bytes)
}

fn too_large(bytes: u64) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, TooLarge { bytes })
}

fn not_a_regular_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "storage path is not a regular file",
    )
}

/// Opens without ever blocking on a special file or following a symlink. On
/// Windows `FILE_FLAG_OPEN_REPARSE_POINT` opens a symlink or junction itself
/// rather than its target, so the caller's `is_file` recheck rejects it.
fn open_regular_for_read(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

/// Reads at most `max` bytes. An absent file (including one removed while it
/// is being read) is `Ok(None)`; a larger file is an `InvalidData` error that
/// `too_large_bytes` recognises.
pub(crate) fn read_bounded(path: &Path, max: usize) -> io::Result<Option<Vec<u8>>> {
    // Inspect the path itself first: opening a FIFO or device for reading can
    // block forever, and a symlink could point at one. Only a regular file is
    // ever opened.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(not_a_regular_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let file = match open_regular_for_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // The path may have been swapped between the check and the open, so the
    // opened handle is verified again.
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(not_a_regular_file());
    }
    let length = metadata.len();
    if length > max as u64 {
        return Err(too_large(length));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(Some(bytes))
}

/// Atomically replaces `path` with `bytes`: a unique owner-only (0600 on unix)
/// temporary file in the same directory is written, synced, renamed over the
/// target, and the parent directory is synced. The temporary file is removed
/// on any failure. The target is replaced, never followed or written through.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_directories(parent)?;

    let (temporary_path, mut temporary_file) = create_temporary_file(path, parent)?;
    let result = (|| -> io::Result<()> {
        temporary_file.write_all(bytes)?;
        sync_durably(&temporary_file)?;
        drop(temporary_file);
        rename_over(&temporary_path, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    #[cfg(unix)]
    {
        // Persist the rename itself. Best effort: the data is already durable
        // and visible, so a directory sync failure must not fail the save.
        if let Ok(directory) = fs::File::open(parent) {
            let _ = sync_durably(&directory);
        }
    }
    Ok(())
}

/// Renames over the target. On Windows an antivirus scanner or indexer can
/// briefly hold either file open, which surfaces as `PermissionDenied`, so the
/// rename is retried a few times; other errors are returned immediately.
#[cfg(windows)]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied && attempt < RENAME_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(RENAME_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

/// Renames over the target (atomic on unix).
#[cfg(not(windows))]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Creates missing directories owner-only (0700 on unix). Existing directories
/// keep whatever permissions their owner chose.
fn create_private_directories(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(directory)
    }
}

/// Flushes file contents to stable storage. `sync_all` on macOS only reaches
/// the drive cache, so `F_FULLFSYNC` is requested first, falling back to
/// `sync_all` where the filesystem does not support it.
fn sync_durably(file: &fs::File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fcntl(F_FULLFSYNC)` takes no pointer arguments and the file
        // descriptor is valid for the lifetime of `file`.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == 0 {
            return Ok(());
        }
    }
    file.sync_all()
}

fn create_temporary_file(path: &Path, parent: &Path) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let temporary = temporary_path(path, parent);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique storage temporary file",
    ))
}

fn temporary_path(path: &Path, parent: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("storage"))
        .to_string_lossy();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{file_name}.{}.{timestamp}.{sequence}.tmp",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::{read_bounded, too_large_bytes, write_private_atomic};
    use crate::testing::TemporaryDirectory;

    #[test]
    fn write_creates_parents_replaces_content_and_leaves_no_temporaries() {
        let directory = TemporaryDirectory::new("atomic-file");
        let path = directory.file("nested", "value.json");

        write_private_atomic(&path, b"first").expect("first write must succeed");
        write_private_atomic(&path, b"second").expect("replacement must succeed");

        assert_eq!(std::fs::read(&path).expect("file must exist"), b"second");
        let leftovers = std::fs::read_dir(path.parent().expect("has parent"))
            .expect("directory must list")
            .count();
        assert_eq!(leftovers, 1);
    }

    #[cfg(unix)]
    #[test]
    fn write_is_owner_only_and_does_not_follow_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TemporaryDirectory::new("atomic-file");
        let victim = directory.file("", "victim");
        std::fs::write(&victim, b"keep").expect("victim must write");
        let link = directory.file("", "link");
        symlink(&victim, &link).expect("symlink must create");

        write_private_atomic(&link, b"secret").expect("write must succeed");

        assert_eq!(std::fs::read(&victim).expect("victim readable"), b"keep");
        let mode = std::fs::metadata(&link)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn read_distinguishes_absent_present_and_oversize_files() {
        let directory = TemporaryDirectory::new("atomic-file");
        let path = directory.file("", "value");

        assert!(read_bounded(&path, 8).expect("absent is fine").is_none());
        std::fs::write(&path, b"12345678").expect("write");
        assert_eq!(
            read_bounded(&path, 8).expect("at limit").as_deref(),
            Some(&b"12345678"[..])
        );
        std::fs::write(&path, b"123456789").expect("write");
        let error = read_bounded(&path, 8).expect_err("over limit must fail");
        assert_eq!(too_large_bytes(&error), Some(9));
    }

    #[test]
    fn read_of_a_directory_is_an_ordinary_io_error() {
        let directory = TemporaryDirectory::new("atomic-file");
        let error = read_bounded(directory.path(), 8).expect_err("directory is not a file");
        assert_eq!(too_large_bytes(&error), None);
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_a_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let directory = TemporaryDirectory::new("atomic-file");
        let path = directory.file("", "pipe");
        let name = CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
        // SAFETY: `name` is a valid NUL-terminated path for the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read_bounded(&path, 8));
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO must not hang");
        assert_eq!(
            result.expect_err("a FIFO is not a regular file").kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_a_symlink_and_missing_parents_are_owner_only() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TemporaryDirectory::new("atomic-file");
        let target = directory.file("", "target");
        std::fs::write(&target, b"data").expect("write");
        let link = directory.file("", "link");
        symlink(&target, &link).expect("symlink");
        assert!(read_bounded(&link, 8).is_err());

        let nested = directory.file("a/b", "value.json");
        write_private_atomic(&nested, b"x").expect("write creates parents");
        for relative in ["a", "a/b"] {
            let mode = std::fs::metadata(directory.path().join(relative))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{relative}");
        }
    }
}
