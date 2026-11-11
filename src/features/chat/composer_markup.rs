//! Pure text edits behind the remote composer's formatting toolbar.
//!
//! The composer keeps markdown-lite source text; the toolbar only inserts
//! the markers `ComposedBody::from_markdown_lite` understands. Positions are
//! `char` indices, matching egui's text cursors.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wrap {
    Bold,
    Italic,
    Code,
    Link,
}

const LINK_PLACEHOLDER: &str = "https://";

impl Wrap {
    const fn markers(self) -> (&'static str, &'static str) {
        match self {
            Self::Bold => ("**", "**"),
            Self::Italic => ("_", "_"),
            Self::Code => ("`", "`"),
            Self::Link => ("[", "]("),
        }
    }
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(index, _)| index)
}

/// Wraps the selection `[start, end)` (in either order) and returns the new
/// text with the selection to leave in place. An empty selection inserts an
/// empty pair with the cursor between the markers; a link selects its URL
/// placeholder so the user can type over it.
pub(super) fn wrap_selection(
    text: &str,
    start: usize,
    end: usize,
    wrap: Wrap,
) -> (String, usize, usize) {
    let count = text.chars().count();
    let (start, end) = (start.min(end).min(count), start.max(end).min(count));
    let (from, to) = (byte_index(text, start), byte_index(text, end));
    let selected = &text[from..to];
    let (open, close) = wrap.markers();
    let mut result = String::with_capacity(text.len() + 16);
    result.push_str(&text[..from]);
    result.push_str(open);
    result.push_str(selected);
    result.push_str(close);
    let selected_chars = selected.chars().count();
    let open_chars = open.chars().count();
    let (new_start, new_end) = if wrap == Wrap::Link {
        result.push_str(LINK_PLACEHOLDER);
        result.push(')');
        let url_start = start + open_chars + selected_chars + close.chars().count();
        (url_start, url_start + LINK_PLACEHOLDER.chars().count())
    } else if selected.is_empty() {
        let inside = start + open_chars;
        (inside, inside)
    } else {
        (start + open_chars, start + open_chars + selected_chars)
    };
    result.push_str(&text[to..]);
    (result, new_start, new_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_selection_and_keeps_it_selected() {
        assert_eq!(
            wrap_selection("say hello now", 4, 9, Wrap::Bold),
            ("say **hello** now".into(), 6, 11)
        );
        assert_eq!(
            wrap_selection("x", 0, 1, Wrap::Italic),
            ("_x_".into(), 1, 2)
        );
        assert_eq!(
            wrap_selection("run it", 4, 6, Wrap::Code),
            ("run `it`".into(), 5, 7)
        );
    }

    #[test]
    fn an_empty_selection_inserts_a_pair_around_the_cursor() {
        assert_eq!(
            wrap_selection("ab", 1, 1, Wrap::Bold),
            ("a****b".into(), 3, 3)
        );
        assert_eq!(wrap_selection("", 0, 0, Wrap::Code), ("``".into(), 1, 1));
    }

    #[test]
    fn a_link_selects_its_url_placeholder() {
        let (text, start, end) = wrap_selection("see docs", 4, 8, Wrap::Link);
        assert_eq!(text, "see [docs](https://)");
        assert_eq!(
            text.chars()
                .skip(start)
                .take(end - start)
                .collect::<String>(),
            "https://"
        );
    }

    #[test]
    fn reversed_and_out_of_range_selections_are_clamped() {
        assert_eq!(wrap_selection("abc", 2, 0, Wrap::Italic).0, "_ab_c");
        assert_eq!(wrap_selection("abc", 1, 99, Wrap::Italic).0, "a_bc_");
    }

    #[test]
    fn multibyte_text_is_indexed_by_char() {
        assert_eq!(
            wrap_selection("é日本x", 1, 3, Wrap::Bold),
            ("é**日本**x".into(), 3, 5)
        );
    }

    #[test]
    fn wrapped_text_parses_back_to_the_marks() {
        use crate::integrations::ComposedBody;
        for wrap in [Wrap::Bold, Wrap::Italic, Wrap::Code] {
            let (text, ..) = wrap_selection("a word b", 2, 6, wrap);
            let body = ComposedBody::from_markdown_lite(&text);
            assert!(!body.is_plain(), "{text}");
            assert_eq!(body.plain_projection(), "a word b");
        }
        let (text, ..) = wrap_selection("a docs b", 2, 6, Wrap::Link);
        let linked = text.replace("https://", "https://example.com");
        assert!(!ComposedBody::from_markdown_lite(&linked).is_plain());
    }
}
