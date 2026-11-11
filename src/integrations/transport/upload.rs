//! Chat file attachments as they cross the integration seam.
//!
//! A file is picked locally, uploaded to the signed-in user's OneDrive
//! ("Microsoft Teams Chat Files"), and then referenced from a chat message.
//! The picked path exists only inside [`LocalUploadSource`] for the duration
//! of one upload job: it is never persisted, logged, or shown. The adapter
//! hands back an opaque [`UploadedFileHandle`]; the drive item ID, the
//! `webDavUrl`, and every other provider value stay in the adapter's private
//! registry.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The largest file this build uploads with one request. Graph accepts more on
/// a simple PUT, but a smaller bound keeps one request short and retryable.
pub(crate) const MAX_CHAT_UPLOAD_BYTES: u64 = 4 * 1024 * 1024;

/// The largest file this build uploads at all: larger than
/// [`MAX_CHAT_UPLOAD_BYTES`] it goes through an upload session, one fragment
/// per worker job.
pub(crate) const MAX_CHAT_UPLOAD_SESSION_BYTES: u64 = 100 * 1024 * 1024;

/// One session fragment: a multiple of 320 KiB (Graph requires it for every
/// fragment but the last) and far below the 60 MiB per-request limit.
pub(crate) const UPLOAD_FRAGMENT_BYTES: u64 = 12 * 320 * 1024;

/// Graph file-name limit, in characters.
const MAX_UPLOAD_NAME_CHARS: usize = 255;

/// The rejection text for a name that already exists in the chat-files
/// folder. The feature recognizes it to offer a rename instead of a retry.
pub(crate) const UPLOAD_NAME_CONFLICT: &str =
    "a file with that name is already in your Teams chat files";

/// Why a file name cannot be used in OneDrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UploadNameError {
    Empty,
    TooLong,
    ReservedCharacter,
    EdgeDotOrSpace,
    ReservedName,
}

impl UploadNameError {
    pub(crate) const fn user_message(self) -> &'static str {
        match self {
            Self::Empty => "The file has no name.",
            Self::TooLong => "The file name is longer than 255 characters.",
            Self::ReservedCharacter => {
                "The file name contains a character OneDrive does not allow (\\ / : * ? \" < > | # %)."
            }
            Self::EdgeDotOrSpace => "The file name cannot start or end with a dot or a space.",
            Self::ReservedName => "OneDrive does not allow that file name.",
        }
    }
}

/// Characters that are invisible or reorder text (Unicode format characters
/// `Cf`, variation selectors, tag characters, the Mongolian vowel separator and
/// the line and paragraph separators). A file name carrying one can read
/// differently from what is uploaded.
fn is_invisible_or_reordering(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// Validates one OneDrive file name. The adapter calls this again before it
/// builds a route, so a caller cannot bypass it. The length is counted in
/// UTF-16 units, as OneDrive counts it.
pub(crate) fn validate_upload_name(name: &str) -> Result<(), UploadNameError> {
    if name.is_empty() {
        return Err(UploadNameError::Empty);
    }
    if name.encode_utf16().count() > MAX_UPLOAD_NAME_CHARS {
        return Err(UploadNameError::TooLong);
    }
    if name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '%'
            )
            || is_invisible_or_reordering(character)
    }) {
        return Err(UploadNameError::ReservedCharacter);
    }
    if name.starts_with(['.', ' ']) || name.ends_with(['.', ' ']) {
        return Err(UploadNameError::EdgeDotOrSpace);
    }
    // Windows reads `CON .txt` and `CON.txt` alike, so the stem is trimmed of
    // spaces before the device check. COM0 and LPT0 are reserved too.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|digit| matches!(digit.as_bytes(), [b'0'..=b'9']))
    };
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered("COM")
        || numbered("LPT")
        || name.eq_ignore_ascii_case("desktop.ini")
        || name.to_ascii_lowercase().starts_with("_vti_")
    {
        return Err(UploadNameError::ReservedName);
    }
    Ok(())
}

/// Why a picked file cannot be attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UploadPickError {
    NotAFile,
    Unreadable,
    TooLarge,
    BadName(UploadNameError),
}

impl UploadPickError {
    pub(crate) const fn user_message(self) -> &'static str {
        match self {
            Self::NotAFile => "That is not a regular file.",
            Self::Unreadable => "The file could not be read.",
            Self::TooLarge => "Files over 100 MiB cannot be attached.",
            Self::BadName(error) => error.user_message(),
        }
    }
}

/// A local file chosen from the system file dialog. Its path is held only to
/// read the bytes inside one upload job and is never shown, logged, or stored.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LocalUploadSource {
    /// The canonical path at the pick, so no link is followed later.
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    /// The file's device and inode at the pick (unix), so a different file
    /// moved into the same path is refused.
    identity: Option<(u64, u64)>,
    name: String,
}

impl fmt::Debug for LocalUploadSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalUploadSource")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl LocalUploadSource {
    /// Inspects a path returned by the file dialog: a regular file within the
    /// size bound whose name OneDrive accepts.
    pub(crate) fn from_pick(path: PathBuf) -> Result<Self, UploadPickError> {
        // Resolve links once, here: every later open refuses a symlink, and the
        // device and inode below are those of the file that was chosen.
        let canonical = std::fs::canonicalize(&path).map_err(|_| UploadPickError::Unreadable)?;
        let metadata = std::fs::metadata(&canonical).map_err(|_| UploadPickError::Unreadable)?;
        if !metadata.is_file() {
            return Err(UploadPickError::NotAFile);
        }
        if metadata.len() > MAX_CHAT_UPLOAD_SESSION_BYTES {
            return Err(UploadPickError::TooLarge);
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(UploadPickError::BadName(UploadNameError::Empty))?
            .to_owned();
        validate_upload_name(&name).map_err(UploadPickError::BadName)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            Some((metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let identity = None;
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            identity,
            path: canonical,
            name,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    pub(in crate::integrations) fn path(&self) -> &Path {
        &self.path
    }

    pub(in crate::integrations) const fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub(in crate::integrations) const fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        path: PathBuf,
        size: u64,
        modified: Option<SystemTime>,
        name: &str,
    ) -> Self {
        Self {
            path,
            size,
            modified,
            identity: None,
            name: name.into(),
        }
    }
}

/// Which upload session a fragment or a cancellation is for. Opaque: the
/// session's URL, which is a secret, stays in the adapter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadSessionRef(u64);

impl fmt::Debug for UploadSessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadSessionRef")
            .finish_non_exhaustive()
    }
}

impl UploadSessionRef {
    pub(in crate::integrations) const fn new(id: u64) -> Self {
        Self(id)
    }

    pub(in crate::integrations) const fn id(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) const fn for_test(id: u64) -> Self {
        Self(id)
    }
}

/// How far a session upload has got after one fragment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadProgress {
    session: UploadSessionRef,
    sent: u64,
    total: u64,
}

impl fmt::Debug for UploadProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadProgress")
            .field("sent", &self.sent)
            .field("total", &self.total)
            .finish_non_exhaustive()
    }
}

impl UploadProgress {
    pub(in crate::integrations) const fn new(
        session: UploadSessionRef,
        sent: u64,
        total: u64,
    ) -> Self {
        Self {
            session,
            sent,
            total,
        }
    }

    pub(crate) const fn session(&self) -> UploadSessionRef {
        self.session
    }

    #[cfg(test)]
    pub(crate) const fn for_test(session: u64, sent: u64, total: u64) -> Self {
        Self::new(UploadSessionRef(session), sent, total)
    }

    pub(crate) const fn sent(&self) -> u64 {
        self.sent
    }

    #[cfg(test)]
    pub(crate) const fn total(&self) -> u64 {
        self.total
    }
}

/// How many people a file was shared with, and how many it could not be.
/// Counts only: no member ID or name ever crosses the seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShareReport {
    pub(crate) granted: u32,
    pub(crate) failed: u32,
}

/// What an upload command reports: the finished file, or progress of a
/// session upload whose next fragment is a separate command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UploadReceipt {
    Done(UploadedFileHandle),
    Progress(UploadProgress),
    /// The file was shared with the chat's other members.
    Shared(ShareReport),
}

/// An uploaded file the adapter can attach to one message. It is opaque: the
/// drive item, its URLs and ETag stay in the adapter's registry. `Debug`
/// prints only the size.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UploadedFileHandle {
    id: u64,
    name: String,
    size: u64,
    /// The attachment ID the message will carry, so an ambiguous send can be
    /// confirmed by exactly this file. It is the GUID of the file's ETag, not
    /// a route.
    reference: String,
    /// The upload's outcome was unknown and a later look found the file. The
    /// user must choose to attach it.
    recovered: bool,
}

impl fmt::Debug for UploadedFileHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadedFileHandle")
            .field("size", &self.size)
            .field("recovered", &self.recovered)
            .finish_non_exhaustive()
    }
}

impl UploadedFileHandle {
    pub(in crate::integrations) const fn new(
        id: u64,
        name: String,
        size: u64,
        reference: String,
        recovered: bool,
    ) -> Self {
        Self {
            id,
            name,
            size,
            reference,
            recovered,
        }
    }

    pub(in crate::integrations) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn reference_id(&self) -> &str {
        &self.reference
    }

    pub(crate) const fn is_recovered(&self) -> bool {
        self.recovered
    }

    /// The user accepted a recovered file: it is attached like any other.
    pub(crate) fn accepted(mut self) -> Self {
        self.recovered = false;
        self
    }

    #[cfg(test)]
    pub(crate) fn for_test(id: u64, name: &str, reference: &str) -> Self {
        Self::new(id, name.into(), 10, reference.into(), false)
    }

    /// Test-only: a file found after an unknown upload outcome.
    #[cfg(test)]
    pub(crate) fn recovered_for_test(id: u64, name: &str, reference: &str) -> Self {
        Self::new(id, name.into(), 10, reference.into(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_matrix() {
        for ok in [
            "report.pdf",
            "a b.txt",
            "naïve résumé.docx",
            "archive.tar.gz",
            "x",
            "name (1).png",
            "dots.in.the.middle",
        ] {
            assert_eq!(validate_upload_name(ok), Ok(()), "{ok}");
        }
        for (bad, expected) in [
            ("", UploadNameError::Empty),
            (&"a".repeat(256), UploadNameError::TooLong),
            ("a/b", UploadNameError::ReservedCharacter),
            ("a\\b", UploadNameError::ReservedCharacter),
            ("a:b", UploadNameError::ReservedCharacter),
            ("a*b", UploadNameError::ReservedCharacter),
            ("a?b", UploadNameError::ReservedCharacter),
            ("a\"b", UploadNameError::ReservedCharacter),
            ("a<b", UploadNameError::ReservedCharacter),
            ("a>b", UploadNameError::ReservedCharacter),
            ("a|b", UploadNameError::ReservedCharacter),
            ("a#b", UploadNameError::ReservedCharacter),
            ("a%b", UploadNameError::ReservedCharacter),
            ("a\nb", UploadNameError::ReservedCharacter),
            ("a\u{202E}b", UploadNameError::ReservedCharacter),
            ("a\u{200B}b", UploadNameError::ReservedCharacter),
            (".hidden", UploadNameError::EdgeDotOrSpace),
            ("trailing.", UploadNameError::EdgeDotOrSpace),
            (" lead", UploadNameError::EdgeDotOrSpace),
            ("trail ", UploadNameError::EdgeDotOrSpace),
            (".", UploadNameError::EdgeDotOrSpace),
            ("..", UploadNameError::EdgeDotOrSpace),
            ("CON", UploadNameError::ReservedName),
            ("nul.txt", UploadNameError::ReservedName),
            ("com1", UploadNameError::ReservedName),
            ("LPT9.log", UploadNameError::ReservedName),
            ("desktop.ini", UploadNameError::ReservedName),
            ("_vti_cnf", UploadNameError::ReservedName),
        ] {
            assert_eq!(validate_upload_name(bad), Err(expected), "{bad:?}");
        }
        // A 255-character name is the longest accepted.
        assert_eq!(validate_upload_name(&"a".repeat(255)), Ok(()));
        assert_eq!(validate_upload_name("com10.txt"), Ok(()));
    }

    #[test]
    fn debug_never_prints_a_path_a_name_or_a_reference() {
        let source = LocalUploadSource::for_test(
            PathBuf::from("/home/ada/secret-plans/q3.pdf"),
            42,
            None,
            "q3.pdf",
        );
        let debug = format!("{source:?}");
        assert!(!debug.contains("secret-plans") && !debug.contains("q3.pdf"));
        let handle = UploadedFileHandle::for_test(1, "q3.pdf", "GUID-REF");
        let debug = format!("{handle:?}");
        assert!(!debug.contains("q3.pdf") && !debug.contains("GUID-REF"));
    }

    #[test]
    fn a_pick_is_checked_for_kind_size_and_name() {
        let dir =
            std::env::temp_dir().join(format!("teamspace-upload-pick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok.txt");
        std::fs::write(&ok, b"hello").unwrap();
        let source = LocalUploadSource::from_pick(ok).unwrap();
        assert_eq!((source.name(), source.size()), ("ok.txt", 5));
        assert!(source.modified().is_some());
        assert_eq!(
            LocalUploadSource::from_pick(dir.clone()),
            Err(UploadPickError::NotAFile)
        );
        assert_eq!(
            LocalUploadSource::from_pick(dir.join("missing.txt")),
            Err(UploadPickError::Unreadable)
        );
        let bad = dir.join("bad#name.txt");
        std::fs::write(&bad, b"x").unwrap();
        assert_eq!(
            LocalUploadSource::from_pick(bad),
            Err(UploadPickError::BadName(UploadNameError::ReservedCharacter))
        );
        let big = dir.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_CHAT_UPLOAD_SESSION_BYTES + 1).unwrap();
        assert_eq!(
            LocalUploadSource::from_pick(big),
            Err(UploadPickError::TooLarge)
        );
        let exact = dir.join("exact.bin");
        let file = std::fs::File::create(&exact).unwrap();
        file.set_len(MAX_CHAT_UPLOAD_SESSION_BYTES).unwrap();
        assert!(LocalUploadSource::from_pick(exact).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_that_hide_or_reorder_text_or_reach_a_device_are_refused() {
        for (bad, expected) in [
            // COM0 and LPT0 are reserved like the other digits.
            ("COM0", UploadNameError::ReservedName),
            ("lpt0.txt", UploadNameError::ReservedName),
            ("COM0.tar.gz", UploadNameError::ReservedName),
            // A stem with trailing spaces is the device it names.
            ("CON .txt", UploadNameError::ReservedName),
            ("nul   .log", UploadNameError::ReservedName),
            ("com1 .x", UploadNameError::ReservedName),
            ("AUX  ", UploadNameError::EdgeDotOrSpace),
            // Format characters, variation selectors, tags, the Mongolian
            // vowel separator and the line and paragraph separators.
            ("a\u{00AD}b", UploadNameError::ReservedCharacter),
            ("a\u{200B}b", UploadNameError::ReservedCharacter),
            ("a\u{2060}b", UploadNameError::ReservedCharacter),
            ("a\u{2066}b", UploadNameError::ReservedCharacter),
            ("a\u{FEFF}b", UploadNameError::ReservedCharacter),
            ("a\u{FE0F}b", UploadNameError::ReservedCharacter),
            ("a\u{FE00}b", UploadNameError::ReservedCharacter),
            ("a\u{E0100}b", UploadNameError::ReservedCharacter),
            ("a\u{E0001}b", UploadNameError::ReservedCharacter),
            ("a\u{E0041}b", UploadNameError::ReservedCharacter),
            ("a\u{E007F}b", UploadNameError::ReservedCharacter),
            ("a\u{180E}b", UploadNameError::ReservedCharacter),
            ("a\u{2028}b", UploadNameError::ReservedCharacter),
            ("a\u{2029}b", UploadNameError::ReservedCharacter),
            ("a\u{0600}b", UploadNameError::ReservedCharacter),
            ("a\u{1D173}b", UploadNameError::ReservedCharacter),
        ] {
            assert_eq!(validate_upload_name(bad), Err(expected), "{bad:?}");
        }
        // The joiners (ZWJ, ZWNJ) are format characters too, so a name that
        // needs them is refused rather than allowed to hide text.
        for joiner in ["x\u{200D}y", "x\u{200C}y"] {
            assert_eq!(
                validate_upload_name(joiner),
                Err(UploadNameError::ReservedCharacter)
            );
        }
        // Ordinary text is fine.
        for ok in [
            "COM10.txt",
            "CONSOLE.txt",
            "a b.txt",
            "日本語.txt",
            "COMM.txt",
        ] {
            assert_eq!(validate_upload_name(ok), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn the_length_is_counted_in_utf16_units() {
        // 255 units is the longest; an emoji is two.
        assert_eq!(validate_upload_name(&"a".repeat(255)), Ok(()));
        assert_eq!(
            validate_upload_name(&"a".repeat(256)),
            Err(UploadNameError::TooLong)
        );
        let emoji = "\u{1F600}";
        assert_eq!(emoji.encode_utf16().count(), 2);
        // 127 emoji and one letter is exactly 255 units.
        assert_eq!(
            validate_upload_name(&format!("{}a", emoji.repeat(127))),
            Ok(())
        );
        // 128 emoji is 128 characters but 256 units.
        assert_eq!(
            validate_upload_name(&emoji.repeat(128)),
            Err(UploadNameError::TooLong)
        );
    }
}
