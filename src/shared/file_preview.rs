//! Provider-neutral limits for in-memory file previews.
//!
//! These caps are intentionally shared by the local Files surface and remote
//! adapters. They bound only transient preview data; no caller may treat them
//! as permission to persist, upload, or otherwise reuse the selected bytes.

/// Maximum source bytes retained for a safe image preview.
pub(crate) const MAX_IMAGE_PREVIEW_BYTES: u64 = 12 * 1024 * 1024;

/// Maximum decoded pixels accepted for an image preview.
pub(crate) const MAX_IMAGE_PREVIEW_PIXELS: u64 = 16 * 1024 * 1024;

/// Maximum UTF-8 source bytes retained for a text preview.
pub(crate) const MAX_TEXT_PREVIEW_BYTES: u64 = 256 * 1024;

/// A process-unique image-cache key for the egui `bytes://` URI of a preview.
///
/// The key mixes a per-process random hash state, a monotonic counter, the
/// length and the bytes. File content is attacker-influenced, so a plain
/// content hash with fixed keys would let two crafted files share a key and
/// alias each other's cached texture. A secret hash state plus a counter that
/// makes every delivery distinct removes that: the same bytes delivered twice
/// intentionally get different keys, because the preview owner forgets the old
/// image when it is dismissed or replaced.
pub(crate) fn next_preview_cache_key(bytes: &[u8]) -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static STATE: OnceLock<RandomState> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = STATE.get_or_init(RandomState::new).build_hasher();
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    bytes.len().hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Whether a file extension (without the dot, any case) is on the shared
/// text-preview allow-list. Local and remote previews both use it so a
/// provider MIME label alone never widens what is rendered as text.
pub(crate) fn is_text_preview_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "txt"
            | "md"
            | "markdown"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "csv"
            | "log"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "js"
            | "ts"
            | "rs"
            | "py"
            | "sh"
            | "zsh"
            | "sql"
    )
}

/// Whether a file name's final extension is on the text-preview allow-list.
pub(crate) fn has_text_preview_extension(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, extension)| !stem.is_empty() && is_text_preview_extension(extension))
}

#[cfg(test)]
mod tests {
    use super::{has_text_preview_extension, is_text_preview_extension, next_preview_cache_key};

    #[test]
    fn cache_keys_are_unique_per_delivery_even_for_identical_bytes() {
        let mut keys = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            assert!(keys.insert(next_preview_cache_key(b"same bytes")));
        }
        assert_ne!(next_preview_cache_key(b"a"), next_preview_cache_key(b"b"));
    }

    #[test]
    fn text_extension_allow_list_is_case_insensitive_and_extension_only() {
        assert!(is_text_preview_extension("TXT"));
        assert!(has_text_preview_extension("Notes.Md"));
        assert!(!has_text_preview_extension("archive.zip"));
        assert!(!has_text_preview_extension("txt"));
        assert!(!has_text_preview_extension(".txt"));
    }
}
