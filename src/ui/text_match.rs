//! Case-insensitive substring matching that reports ranges in the original
//! text, so callers can highlight and rank without slicing off a char
//! boundary.

use crate::ui::theme;
use eframe::egui::{self, text::LayoutJob};
use std::ops::Range;

/// How well a text matches a query. Declared best-first so it sorts naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchRank {
    Exact,
    Prefix,
    Contains,
}

/// Lowercased chars of `text`, each tagged with the byte range of the original
/// char it came from (a char may lowercase to several).
fn fold(text: &str) -> Vec<(char, Range<usize>)> {
    text.char_indices()
        .flat_map(|(start, ch)| {
            let end = start + ch.len_utf8();
            ch.to_lowercase().map(move |lower| (lower, start..end))
        })
        .collect()
}

/// Non-overlapping byte ranges of `text` matching `needle`, ignoring case.
/// Every range starts and ends on a char boundary of `text`.
pub(crate) fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = needle.trim().chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let folded = fold(text);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut index = 0;
    while index + needle.len() <= folded.len() {
        let window = &folded[index..index + needle.len()];
        if window.iter().map(|(c, _)| *c).eq(needle.iter().copied()) {
            let start = window[0].1.start;
            let end = window[window.len() - 1].1.end;
            match ranges.last_mut() {
                Some(last) if start < last.end => last.end = last.end.max(end),
                _ => ranges.push(start..end),
            }
            index += needle.len();
        } else {
            index += 1;
        }
    }
    ranges
}

/// Rank of `text` for `query`: exact title, then prefix, then anywhere else.
pub(crate) fn rank(text: &str, query: &str) -> MatchRank {
    let fold_all =
        |value: &str| -> String { value.trim().chars().flat_map(char::to_lowercase).collect() };
    let (text, query) = (fold_all(text), fold_all(query));
    if text == query {
        MatchRank::Exact
    } else if text.starts_with(&query) {
        MatchRank::Prefix
    } else {
        MatchRank::Contains
    }
}

/// A layout job painting `ranges` of `text` in the search-hit style.
pub(crate) fn highlighted(
    text: &str,
    ranges: &[Range<usize>],
    base: egui::Color32,
    size: f32,
) -> LayoutJob {
    let normal = egui::TextFormat {
        font_id: theme::regular(size),
        color: base,
        ..Default::default()
    };
    let hit = egui::TextFormat {
        font_id: theme::semibold(size),
        color: theme::search_hit(),
        background: theme::search_hit_bg(),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        if range.start > cursor {
            job.append(&text[cursor..range.start], 0.0, normal.clone());
        }
        job.append(&text[range.clone()], 0.0, hit.clone());
        cursor = range.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, normal);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::{MatchRank, highlighted, match_ranges, rank};

    #[test]
    fn ranges_cover_the_original_text_case_insensitively() {
        assert_eq!(match_ranges("Design Review", "review"), vec![7..13]);
        assert_eq!(match_ranges("aXbxc", "x"), vec![1..2, 3..4]);
        assert!(match_ranges("abc", "  ").is_empty());
        assert!(match_ranges("abc", "z").is_empty());
    }

    #[test]
    fn multibyte_and_expanding_lowercase_stay_on_char_boundaries() {
        let text = "LÁSZLÓ İstanbul Straße";
        for needle in ["lászló", "szl", "i", "i̇stanbul", "ß", "STRASSE", "e"] {
            for range in match_ranges(text, needle) {
                assert!(text.is_char_boundary(range.start));
                assert!(text.is_char_boundary(range.end));
                let _ = &text[range];
            }
        }
        assert_eq!(match_ranges("LÁSZLÓ", "lászló"), vec![0..8]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(match_ranges("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn rank_orders_exact_prefix_contains() {
        assert_eq!(rank("Chat", "chat"), MatchRank::Exact);
        assert_eq!(rank("Chatroom", "chat"), MatchRank::Prefix);
        assert_eq!(rank("My chat", "chat"), MatchRank::Contains);
        assert!(MatchRank::Exact < MatchRank::Prefix && MatchRank::Prefix < MatchRank::Contains);
    }

    #[test]
    fn highlighted_text_is_the_original_text() {
        let text = "Design Review";
        let job = highlighted(
            text,
            &match_ranges(text, "sign"),
            crate::ui::theme::fg1(),
            12.0,
        );
        assert_eq!(job.text, text);
        assert_eq!(job.sections.len(), 3);
        // Out-of-range or misaligned ranges are ignored, never panic.
        let job = highlighted("é", &[1..2, 5..9], crate::ui::theme::fg1(), 12.0);
        assert_eq!(job.text, "é");
    }
}
