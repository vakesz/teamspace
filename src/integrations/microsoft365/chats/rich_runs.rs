//! Converts a Graph chat message's HTML body into formatted runs.
//!
//! Only an allow-list survives: `b`/`strong`, `i`/`em`, `code`, `pre`, `a`
//! (an `https` `href` only), and `p`/`div`/`br` as line breaks. Every other
//! tag is dropped but its text is kept, `script` and `style` content is
//! discarded, and a link whose target is not a valid `https` URL becomes
//! plain text. Malformed or oversized input yields `None`, so the caller
//! falls back to the authoritative plain text.

use super::{
    MAX_PLAIN_MESSAGE_BYTES, MAX_RAW_MESSAGE_BODY_BYTES, bounded_ascii_delimiter, decode_entity,
};
use crate::integrations::microsoft365::text::is_deceptive_format_char;
use crate::integrations::transport::{Inline, Marks, RichRun, SafeHttpsUrl};

const MAX_RUNS: usize = 256;
const MAX_NESTING: usize = 8;
/// An `<a>` tag carries a URL of up to 2 KiB.
pub(super) const MAX_RICH_TAG_BYTES: usize = 2_304;
const MAX_ENTITY_BYTES: usize = 16;

#[derive(Clone, Debug)]
enum Open {
    Bold,
    Italic,
    Code,
    Pre,
    Link(Option<SafeHttpsUrl>),
}

struct Builder {
    runs: Vec<RichRun>,
    stack: Vec<Open>,
    at_line_start: bool,
    /// The last thing written is a collapsed whitespace run's single space.
    last_was_space: bool,
    text_bytes: usize,
    failed: bool,
}

impl Builder {
    fn marks(&self) -> Marks {
        let has = |wanted: fn(&Open) -> bool| self.stack.iter().any(wanted);
        Marks {
            bold: has(|open| matches!(open, Open::Bold)),
            italic: has(|open| matches!(open, Open::Italic)),
            code: has(|open| matches!(open, Open::Code | Open::Pre)),
        }
    }

    fn link(&self) -> Option<SafeHttpsUrl> {
        self.stack.iter().find_map(|open| match open {
            Open::Link(Some(url)) => Some(url.clone()),
            _ => None,
        })
    }

    fn in_pre(&self) -> bool {
        self.stack.iter().any(|open| matches!(open, Open::Pre))
    }

    fn push_run(&mut self, text: &str) {
        if text.is_empty() || self.failed {
            return;
        }
        self.text_bytes += text.len();
        if self.text_bytes > MAX_PLAIN_MESSAGE_BYTES {
            self.failed = true;
            return;
        }
        self.at_line_start = false;
        let marks = self.marks();
        let link = self.link();
        match (self.runs.last_mut(), &link) {
            (
                Some(Inline::Text {
                    text: last,
                    marks: last_marks,
                }),
                None,
            ) if *last_marks == marks => {
                last.push_str(text);
                return;
            }
            (
                Some(Inline::Link {
                    text: last,
                    marks: last_marks,
                    url: last_url,
                }),
                Some(url),
            ) if *last_marks == marks && last_url == url => {
                last.push_str(text);
                return;
            }
            _ => {}
        }
        if self.runs.len() >= MAX_RUNS {
            self.failed = true;
            return;
        }
        self.runs.push(match link {
            Some(url) => Inline::Link {
                text: text.into(),
                marks,
                url,
            },
            None => Inline::Text {
                text: text.into(),
                marks,
            },
        });
    }

    /// Drops a collapsed space left at the end of a line.
    fn trim_trailing_space(&mut self) {
        if !self.last_was_space {
            return;
        }
        self.last_was_space = false;
        self.text_bytes = self.text_bytes.saturating_sub(1);
        let emptied = match self.runs.last_mut() {
            Some(Inline::Text { text, .. } | Inline::Link { text, .. }) => {
                text.pop();
                text.is_empty()
            }
            Some(Inline::Mention { .. }) | None => false,
        };
        if emptied {
            self.runs.pop();
        }
    }

    /// A line break between blocks; never starts or doubles a line.
    fn block_break(&mut self) {
        if !self.at_line_start {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.trim_trailing_space();
        // Line breaks are unformatted so formatting never spans them.
        let saved = std::mem::take(&mut self.stack);
        self.push_run("\n");
        self.stack = saved;
        self.last_was_space = false;
        self.at_line_start = true;
    }

    fn text(&mut self, chunk: &str) {
        if self.in_pre() {
            self.last_was_space = false;
            for line in chunk.split_inclusive('\n') {
                let (body, newline) = line
                    .strip_suffix('\n')
                    .map_or((line, false), |body| (body, true));
                self.push_run(body);
                if newline {
                    self.newline();
                }
            }
            return;
        }
        for character in chunk.chars() {
            if character.is_whitespace() {
                // Written in the style current at this point, so a space
                // never ends up inside a neighbouring mark or link.
                if !self.at_line_start && !self.last_was_space {
                    self.push_run(" ");
                    self.last_was_space = true;
                }
                continue;
            }
            if character.is_control() || is_deceptive_format_char(character) {
                continue;
            }
            self.last_was_space = false;
            let mut buffer = [0_u8; 4];
            self.push_run(character.encode_utf8(&mut buffer));
        }
    }
}

struct Tag<'a> {
    name: String,
    closing: bool,
    attributes: &'a str,
}

fn parse_tag(inner: &str) -> Tag<'_> {
    let trimmed = inner.trim();
    let closing = trimmed.starts_with('/');
    let body = trimmed.trim_start_matches('/').trim_start();
    let name_end = body
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(body.len());
    Tag {
        name: body[..name_end].to_ascii_lowercase(),
        closing,
        attributes: &body[name_end..],
    }
}

/// The value of the one attribute, which must be `href` in a quoted form.
/// `None` for any other attribute set.
fn sole_href(attributes: &str) -> Option<String> {
    let attributes = attributes.trim().trim_end_matches('/').trim();
    let rest = attributes
        .strip_prefix("href")?
        .trim_start()
        .strip_prefix('=')?;
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let value = rest[1..].strip_suffix(quote)?;
    if value.contains(quote) {
        return None;
    }
    Some(decode_entities(value))
}

fn decode_entities(raw: &str) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < raw.len() {
        if raw.as_bytes()[index] == b'&'
            && let Some(end) = bounded_ascii_delimiter(raw, index + 1, b';', MAX_ENTITY_BYTES)
            && let Some(character) = decode_entity(&raw[index + 1..end])
        {
            out.push(character);
            index = end + 1;
            continue;
        }
        let character = raw[index..].chars().next().expect("in bounds");
        out.push(character);
        index += character.len_utf8();
    }
    out
}

/// Whether `a`'s attributes are exactly a valid `https` `href`.
pub(super) fn is_safe_link_tag(attributes: &str) -> bool {
    sole_href(attributes).is_some_and(|href| SafeHttpsUrl::parse(&href).is_some())
}

/// Converts `raw` to runs, or `None` when it has no formatting worth
/// keeping, is malformed, or exceeds a bound.
pub(super) fn html_to_rich_runs(raw: &str) -> Option<Vec<RichRun>> {
    if raw.len() > MAX_RAW_MESSAGE_BODY_BYTES {
        return None;
    }
    let mut builder = Builder {
        runs: Vec::new(),
        stack: Vec::new(),
        at_line_start: true,
        last_was_space: false,
        text_bytes: 0,
        failed: false,
    };
    let mut omitted: Option<&'static str> = None;
    let mut index = 0;
    while index < raw.len() && !builder.failed {
        let byte = raw.as_bytes()[index];
        if byte == b'<' {
            let close = bounded_ascii_delimiter(raw, index + 1, b'>', MAX_RICH_TAG_BYTES)?;
            let tag = parse_tag(&raw[index + 1..close]);
            index = close + 1;
            if let Some(element) = omitted {
                if tag.closing && tag.name == element {
                    omitted = None;
                }
                continue;
            }
            let self_closing = tag.attributes.trim_end().ends_with('/');
            match (tag.name.as_str(), tag.closing) {
                ("script", false) if !self_closing => omitted = Some("script"),
                ("style", false) if !self_closing => omitted = Some("style"),
                ("br", _) => builder.newline(),
                ("p" | "div" | "li" | "tr" | "ul" | "ol" | "blockquote" | "hr" | "table", _) => {
                    builder.block_break();
                }
                (name @ ("b" | "strong" | "i" | "em" | "code" | "pre" | "a"), false)
                    if !self_closing =>
                {
                    if builder.stack.len() >= MAX_NESTING {
                        return None;
                    }
                    let open = match name {
                        "b" | "strong" => Open::Bold,
                        "i" | "em" => Open::Italic,
                        "code" => Open::Code,
                        "pre" => {
                            builder.block_break();
                            Open::Pre
                        }
                        _ => Open::Link(
                            sole_href(tag.attributes).and_then(|href| SafeHttpsUrl::parse(&href)),
                        ),
                    };
                    builder.stack.push(open);
                }
                (name @ ("b" | "strong" | "i" | "em" | "code" | "pre" | "a"), true) => {
                    let matches = matches!(
                        (name, builder.stack.last()),
                        ("b" | "strong", Some(Open::Bold))
                            | ("i" | "em", Some(Open::Italic))
                            | ("code", Some(Open::Code))
                            | ("pre", Some(Open::Pre))
                            | ("a", Some(Open::Link(_)))
                    );
                    if !matches {
                        return None;
                    }
                    let was_pre = matches!(builder.stack.pop(), Some(Open::Pre));
                    if was_pre {
                        builder.block_break();
                    }
                }
                // Anything else (span, at, img, ...) is transparent.
                _ => {}
            }
            continue;
        }
        if omitted.is_some() {
            index += raw[index..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if byte == b'&'
            && let Some(end) = bounded_ascii_delimiter(raw, index + 1, b';', MAX_ENTITY_BYTES)
            && let Some(character) = decode_entity(&raw[index + 1..end])
        {
            let mut buffer = [0_u8; 4];
            builder.text(character.encode_utf8(&mut buffer));
            index = end + 1;
            continue;
        }
        let next = raw[index..]
            .find(['<', '&'])
            .map_or(raw.len(), |offset| index + offset.max(1));
        builder.text(&raw[index..next]);
        index = next;
    }
    if builder.failed || omitted.is_some() || !builder.stack.is_empty() {
        return None;
    }
    builder.trim_trailing_space();
    // Trailing line breaks are layout, not content.
    while let Some(Inline::Text { text, marks }) = builder.runs.last_mut() {
        if *marks != Marks::default() || !text.ends_with('\n') {
            break;
        }
        text.pop();
        if text.is_empty() {
            builder.runs.pop();
        }
    }
    let formatted = builder.runs.iter().any(|run| match run {
        Inline::Link { .. } | Inline::Mention { .. } => true,
        Inline::Text { marks, .. } => *marks != Marks::default(),
    });
    formatted.then_some(builder.runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Inline {
        Inline::Text {
            text: value.into(),
            marks: Marks::default(),
        }
    }

    fn marked(value: &str, bold: bool, italic: bool, code: bool) -> Inline {
        Inline::Text {
            text: value.into(),
            marks: Marks { bold, italic, code },
        }
    }

    #[test]
    fn allow_list_tags_map_to_marks() {
        let runs = html_to_rich_runs(
            "<p>a <strong>b</strong> <em>c</em> <code>d</code> <b><i>e</i></b></p>",
        )
        .expect("formatted");
        assert_eq!(
            runs,
            vec![
                text("a "),
                marked("b", true, false, false),
                text(" "),
                marked("c", false, true, false),
                text(" "),
                marked("d", false, false, true),
                text(" "),
                marked("e", true, true, false),
            ]
        );
    }

    #[test]
    fn paragraphs_and_breaks_become_newlines_and_edges_are_trimmed() {
        let runs =
            html_to_rich_runs("<p><b>one</b></p><p>two<br>three</p>\n<p></p>").expect("rich");
        assert_eq!(
            runs,
            vec![marked("one", true, false, false), text("\ntwo\nthree")]
        );
    }

    #[test]
    fn pre_keeps_its_line_structure_as_code() {
        let runs = html_to_rich_runs("<pre>a  b\n  c</pre>").expect("rich");
        assert_eq!(
            runs,
            vec![
                marked("a  b", false, false, true),
                text("\n"),
                marked("  c", false, false, true)
            ]
        );
    }

    #[test]
    fn https_links_become_runs_and_other_targets_become_text() {
        let runs = html_to_rich_runs(
            "<a href=\"https://example.com/x?a=1&amp;b=2\">site</a> <b><a href=\"http://example.com\">plain</a></b>",
        )
        .expect("rich");
        assert_eq!(
            runs[0],
            Inline::Link {
                text: "site".into(),
                marks: Marks::default(),
                url: SafeHttpsUrl::parse("https://example.com/x?a=1&b=2").unwrap(),
            }
        );
        assert_eq!(runs[2], marked("plain", true, false, false));
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "http://example.com",
            "https://u:p@example.com",
            "//example.com",
            "/relative",
        ] {
            let html = format!("<b>x</b><a href=\"{bad}\">t</a>");
            let runs = html_to_rich_runs(&html).expect("rich");
            assert!(
                runs.iter().all(|run| matches!(run, Inline::Text { .. })),
                "{bad}"
            );
        }
        // Attributes other than a lone href never make a link.
        let runs =
            html_to_rich_runs("<b>x</b><a href=\"https://example.com\" onclick=\"x()\">t</a>")
                .expect("rich");
        assert!(runs.iter().all(|run| matches!(run, Inline::Text { .. })));
    }

    #[test]
    fn script_style_and_event_attributes_never_survive() {
        let runs = html_to_rich_runs(
            "<b>ok</b><script>alert(1)</script><style>p{}</style><img src=x onerror=alert(1)><span onclick=\"x\">kept</span>",
        )
        .expect("rich");
        let joined: String = runs
            .iter()
            .map(|run| match run {
                Inline::Text { text, .. } | Inline::Link { text, .. } => text.as_str(),
                Inline::Mention { display, .. } => display.as_str(),
            })
            .collect();
        assert_eq!(joined, "okkept");
    }

    #[test]
    fn unformatted_bodies_are_not_rich() {
        assert_eq!(html_to_rich_runs("<p>just text</p><p>more</p>"), None);
        assert_eq!(html_to_rich_runs("plain &amp; simple"), None);
    }

    #[test]
    fn malformed_input_returns_none() {
        for raw in [
            "<b>unclosed",
            "<b>x</i>",
            "</b>",
            "<b>x</b",
            "<script>never closed",
            "<b><i>x</b></i>",
        ] {
            assert_eq!(html_to_rich_runs(raw), None, "{raw}");
        }
    }

    #[test]
    fn bounds_cap_runs_nesting_and_size() {
        let many = "<b>x</b> ".repeat(300);
        assert_eq!(html_to_rich_runs(&many), None);
        let ok = "<b>x</b> ".repeat(100);
        assert!(html_to_rich_runs(&ok).is_some());
        let deep = format!("{}x{}", "<b>".repeat(9), "</b>".repeat(9));
        assert_eq!(html_to_rich_runs(&deep), None);
        let huge = format!("<b>{}</b>", "a".repeat(MAX_RAW_MESSAGE_BODY_BYTES));
        assert_eq!(html_to_rich_runs(&huge), None);
    }

    #[test]
    fn hidden_and_control_characters_are_stripped() {
        let runs = html_to_rich_runs("<b>a\u{202E}b\u{0007}c</b>").expect("rich");
        assert_eq!(runs, vec![marked("abc", true, false, false)]);
    }
}
