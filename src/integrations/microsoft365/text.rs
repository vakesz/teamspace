//! Text and identifier normalization shared by every Microsoft Graph read and
//! mutation adapter.
//!
//! Provider strings are untrusted. Keeping one implementation here means a
//! hardening fix (for example bidirectional-control stripping) reaches every
//! surface instead of only the module that happened to receive it.

/// Upper bound for an opaque provider identifier used as a route segment.
pub(super) const MAX_PROVIDER_IDENTIFIER_BYTES: usize = 2_048;

/// Returns a trimmed provider identifier, or `None` when it is empty, too
/// long, contains a control character, or is a `.`/`..` dot segment that
/// `Url` would silently drop and thereby retarget a request.
pub(super) fn normalized_identifier(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()
        && trimmed.len() <= MAX_PROVIDER_IDENTIFIER_BYTES
        && !matches!(trimmed, "." | "..")
        && !trimmed.chars().any(char::is_control))
    .then(|| trimmed.into())
}

/// Invisible bidirectional controls and zero-width characters that can make
/// provider text render differently from its logical order or hide content.
/// ZWNJ (U+200C) and ZWJ (U+200D) are intentionally kept: they are required
/// by emoji sequences and by several scripts.
pub(super) fn is_deceptive_format_char(character: char) -> bool {
    matches!(
        character,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{061C}'
            | '\u{200B}'
            | '\u{2060}'
            | '\u{FEFF}'
    )
}

/// Converts untrusted provider text into bounded one-line presentation text:
/// deceptive format characters and controls are removed, whitespace is folded,
/// and the result is cut on a character boundary.
pub(super) fn safe_display(value: &str, max_bytes: usize) -> String {
    let filtered = value
        .chars()
        .filter(|character| !is_deceptive_format_char(*character))
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    let compact = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8(&compact, max_bytes).trim().into()
}

pub(super) fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.into();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}

#[cfg(test)]
mod tests {
    use super::{normalized_identifier, safe_display, truncate_utf8};

    #[test]
    fn dot_segments_are_not_valid_provider_identifiers() {
        for value in [".", "..", " .. ", " . ", "", "bad\u{0}id"] {
            assert_eq!(normalized_identifier(value), None, "{value:?}");
        }
        for value in ["a.b", "...", "..a", "19:abc@thread.tacv2", "AAMk=="] {
            assert_eq!(normalized_identifier(value).as_deref(), Some(value));
        }
        assert_eq!(normalized_identifier(&"x".repeat(2_049)), None);
    }

    #[test]
    fn safe_display_strips_bidi_and_zero_width_controls_but_keeps_joiners() {
        let value = "a\u{202A}\u{202E}b\u{2066}\u{2069}c\u{200E}\u{200F}d\u{061C}e\u{200B}f\u{2060}g\u{FEFF}h\u{200D}i\u{200C}j";
        assert_eq!(safe_display(value, 256), "abcdefgh\u{200D}i\u{200C}j");
    }

    #[test]
    fn safe_display_folds_whitespace_and_truncates_on_a_char_boundary() {
        assert_eq!(safe_display("  a \n\t b\u{0007} ", 64), "a b");
        assert_eq!(truncate_utf8("ééé", 3), "é");
        assert_eq!(safe_display("ééé", 3), "é");
    }

    /// The display names that feed team, channel, file, event and reply rows
    /// must all be protected: a right-to-left override in a file name is the
    /// classic way to disguise an extension.
    #[test]
    fn a_right_to_left_override_cannot_reach_any_display_surface() {
        let disguised = "invoice\u{202E}fdp.exe";
        assert_eq!(safe_display(disguised, 256), "invoicefdp.exe");
    }
}
