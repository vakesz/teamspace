//! The structured body of an outgoing chat message.
//!
//! A [`ComposedBody`] is what the feature hands the adapter instead of a raw
//! string, so the adapter (not the UI) decides how it reaches the wire. A
//! body with no formatting is *plain* and keeps the exact `text` wire form;
//! anything else is rendered by the adapter into an allow-listed HTML subset.
//!
//! The "markdown-lite" parser is total: every input string yields a body, and
//! a marker that is not balanced stays literal text.

use super::chat::{MAX_MENTIONS_PER_MESSAGE, MentionHandle};
use url::Url;

const MAX_LINK_URL_BYTES: usize = 2 * 1024;
const MAX_BLOCKS: usize = 1024;
const MAX_INLINES: usize = 4096;

/// A validated absolute `https` destination: a host, no user info, bounded.
/// Only this type can become an `href`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeHttpsUrl(String);

impl SafeHttpsUrl {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_LINK_URL_BYTES
            || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return None;
        }
        // Require the literal `https://` shape before the forgiving parser
        // can normalize something like `https:/host`.
        let (scheme, rest) = trimmed.split_once(':')?;
        if !scheme.eq_ignore_ascii_case("https")
            || !rest.starts_with("//")
            || rest
                .get(2..)
                .is_none_or(|authority| authority.starts_with('/'))
        {
            return None;
        }
        // An empty or present user-info section is never acceptable.
        let authority_end = rest[2..]
            .find(['/', '?', '#'])
            .map_or(rest.len(), |end| end + 2);
        if rest[2..authority_end].contains('@') {
            return None;
        }
        let url = Url::parse(trimmed).ok()?;
        (url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none())
        .then(|| Self(url.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The host, for showing where a link goes before it is opened.
    pub(crate) fn host(&self) -> Option<String> {
        Url::parse(&self.0)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
    }
}

/// Inline emphasis. Colours and sizes are chosen by the renderer, never here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Marks {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) code: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inline {
    Text {
        text: String,
        marks: Marks,
    },
    Link {
        text: String,
        marks: Marks,
        url: SafeHttpsUrl,
    },
    /// A mention of one member of the chat, by opaque handle. Only a composed
    /// body has these; the adapter resolves the handle when it sends.
    Mention {
        handle: MentionHandle,
        display: String,
    },
}

impl Inline {
    /// The text as Graph reads it back: a mention is just the name, since the
    /// `at` element's content carries no `@`.
    fn text(&self) -> &str {
        match self {
            Self::Text { text, .. } | Self::Link { text, .. } => text,
            Self::Mention { display, .. } => display,
        }
    }

    fn is_unformatted_text(&self) -> bool {
        matches!(self, Self::Text { marks, .. } if *marks == Marks::default())
    }
}

/// A member picked in the composer: the handle to send and the name that was
/// inserted after the `@`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraftMention {
    pub(crate) handle: MentionHandle,
    pub(crate) display: String,
}

/// Stands in for a mention while the draft is parsed. It is a private-use
/// character, so the parser never gives it meaning.
const MENTION_PLACEHOLDER: char = '\u{E000}';

/// `(start, end, picked index)` of each intact `@Display`, left to right.
fn mention_spans(draft: &str, picked: &[DraftMention]) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    let mut previous: Option<char> = None;
    let mut skip_until = 0;
    for (index, character) in draft.char_indices() {
        let before = previous.replace(character);
        if index < skip_until || character != '@' || spans.len() >= MAX_MENTIONS_PER_MESSAGE {
            continue;
        }
        if before.is_some_and(char::is_alphanumeric) {
            continue;
        }
        let rest = &draft[index + 1..];
        let best = picked
            .iter()
            .enumerate()
            .filter(|(_, member)| {
                !member.display.is_empty()
                    && rest.starts_with(member.display.as_str())
                    && !rest[member.display.len()..]
                        .chars()
                        .next()
                        .is_some_and(char::is_alphanumeric)
            })
            .max_by_key(|(_, member)| member.display.len());
        if let Some((which, member)) = best {
            let end = index + 1 + member.display.len();
            spans.push((index, end, which));
            skip_until = end;
        }
    }
    spans
}

/// Puts back the original `@Display` text for placeholders that ended up in
/// code or a link, where a mention has no meaning.
fn restore_literals(
    text: &str,
    spans: &[(usize, usize, usize)],
    picked: &[DraftMention],
    next: &mut usize,
) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, piece) in text.split(MENTION_PLACEHOLDER).enumerate() {
        if index > 0
            && let Some(&(_, _, which)) = spans.get(*next)
        {
            *next += 1;
            out.push('@');
            out.push_str(&picked[which].display);
        }
        out.push_str(piece);
    }
    out
}

/// A received run of message text, in the same shape as a composed one so
/// one renderer and one edit path serve both. Paragraph breaks are `"\n"`
/// inside unformatted text runs.
pub(crate) type RichRun = Inline;

/// One paragraph per line; a blank line is an empty block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComposedBody {
    blocks: Vec<Vec<Inline>>,
}

impl ComposedBody {
    /// A body with no formatting: `text` is taken literally, one block per
    /// line, with no markdown interpretation.
    pub(crate) fn plain(text: &str) -> Self {
        Self {
            blocks: text
                .split('\n')
                .map(|line| {
                    if line.is_empty() {
                        Vec::new()
                    } else {
                        vec![Inline::Text {
                            text: line.into(),
                            marks: Marks::default(),
                        }]
                    }
                })
                .collect(),
        }
    }

    /// Parses `**bold**`, `_italic_`, `` `code` `` and `[text](https://url)`.
    /// Total: unbalanced or invalid markers remain literal text.
    pub(crate) fn from_markdown_lite(source: &str) -> Self {
        Self {
            blocks: source
                .split('\n')
                .map(|line| parse_inline(line, Marks::default()))
                .collect(),
        }
    }

    /// Rebuilds a body from received runs, splitting paragraphs on `"\n"`.
    /// `None` when a link spans a line break or the result is out of bounds.
    pub(crate) fn from_runs(runs: &[RichRun]) -> Option<Self> {
        let mut blocks: Vec<Vec<Inline>> = vec![Vec::new()];
        for run in runs {
            match run {
                Inline::Text { text, marks } => {
                    for (index, part) in text.split('\n').enumerate() {
                        if index > 0 {
                            blocks.push(Vec::new());
                        }
                        if let Some(block) = blocks.last_mut() {
                            push_text(block, part, *marks);
                        }
                    }
                }
                Inline::Link { text, .. } => {
                    if text.contains('\n') {
                        return None;
                    }
                    blocks.last_mut()?.push(run.clone());
                }
                Inline::Mention { .. } => blocks.last_mut()?.push(run.clone()),
            }
        }
        let body = Self { blocks };
        body.within_bounds().then_some(body)
    }

    /// The markdown-lite source that parses back to this body, when it can be
    /// represented at all. Callers must check `from_markdown_lite` of the
    /// result equals `self`: text containing marker characters, a padded
    /// mark, or a code link cannot round trip.
    pub(crate) fn to_markdown_lite(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.iter().map(inline_markdown).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The body as display runs, with `"\n"` between paragraphs.
    pub(crate) fn to_runs(&self) -> Vec<RichRun> {
        let mut runs = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                push_text(&mut runs, "\n", Marks::default());
            }
            runs.extend(block.iter().cloned());
        }
        runs
    }

    pub(crate) fn blocks(&self) -> &[Vec<Inline>] {
        &self.blocks
    }

    /// The visible text with every mark and link target removed, one line
    /// per block. For a plain body this is exactly the source text.
    pub(crate) fn plain_projection(&self) -> String {
        self.blocks
            .iter()
            .map(|block| block.iter().map(Inline::text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the body carries no formatting at all and may use the `text`
    /// wire form.
    pub(crate) fn is_plain(&self) -> bool {
        self.blocks
            .iter()
            .all(|block| block.len() <= 1 && block.iter().all(Inline::is_unformatted_text))
    }

    /// Structural bounds, checked before the adapter renders anything.
    pub(crate) fn within_bounds(&self) -> bool {
        self.blocks.len() <= MAX_BLOCKS
            && self.blocks.iter().map(Vec::len).sum::<usize>() <= MAX_INLINES
            && self.mention_refs().len() <= MAX_MENTIONS_PER_MESSAGE
    }

    /// Every mention in document order, a repeated person repeating.
    pub(crate) fn mention_refs(&self) -> Vec<(MentionHandle, &str)> {
        self.blocks
            .iter()
            .flatten()
            .filter_map(|inline| match inline {
                Inline::Mention { handle, display } => Some((*handle, display.as_str())),
                _ => None,
            })
            .collect()
    }

    /// The handles of every mention, in document order.
    pub(crate) fn mentions(&self) -> Vec<MentionHandle> {
        self.mention_refs()
            .into_iter()
            .map(|(handle, _)| handle)
            .collect()
    }

    /// Parses markdown-lite while turning each intact `@Display` of a picked
    /// member into a mention. A mention must start at a word boundary and end
    /// at one; the longest matching name wins; at most
    /// [`MAX_MENTIONS_PER_MESSAGE`] count, and the rest stay text. Marks may
    /// span a mention. A mention inside code or a link stays literal text.
    pub(crate) fn compose(draft: &str, picked: &[DraftMention]) -> Self {
        if picked.is_empty() || draft.contains(MENTION_PLACEHOLDER) {
            return Self::from_markdown_lite(draft);
        }
        let spans = mention_spans(draft, picked);
        if spans.is_empty() {
            return Self::from_markdown_lite(draft);
        }
        // Swap each mention for a character the parser treats as ordinary
        // text, parse once, then put the mentions back in order.
        let mut substituted = String::with_capacity(draft.len());
        let mut last = 0;
        for &(start, end, _) in &spans {
            substituted.push_str(&draft[last..start]);
            substituted.push(MENTION_PLACEHOLDER);
            last = end;
        }
        substituted.push_str(&draft[last..]);
        let mut body = Self::from_markdown_lite(&substituted);
        let mut next = 0;
        for block in &mut body.blocks {
            let mut rebuilt = Vec::new();
            for inline in std::mem::take(block) {
                match inline {
                    Inline::Text { text, marks } if !marks.code => {
                        for (index, piece) in text.split(MENTION_PLACEHOLDER).enumerate() {
                            if index > 0 {
                                let Some(&(_, _, which)) = spans.get(next) else {
                                    return Self::from_markdown_lite(draft);
                                };
                                next += 1;
                                rebuilt.push(Inline::Mention {
                                    handle: picked[which].handle,
                                    display: picked[which].display.clone(),
                                });
                            }
                            push_text(&mut rebuilt, piece, marks);
                        }
                    }
                    Inline::Text { text, marks } => {
                        let text = restore_literals(&text, &spans, picked, &mut next);
                        push_text(&mut rebuilt, &text, marks);
                    }
                    Inline::Link { text, marks, url } => {
                        let text = restore_literals(&text, &spans, picked, &mut next);
                        rebuilt.push(Inline::Link { text, marks, url });
                    }
                    mention @ Inline::Mention { .. } => rebuilt.push(mention),
                }
            }
            *block = rebuilt;
        }
        // A placeholder swallowed by a link target breaks the pairing; fall
        // back to no mentions rather than attach one to the wrong text.
        if next != spans.len() {
            return Self::from_markdown_lite(draft);
        }
        body
    }
}

fn inline_markdown(inline: &Inline) -> String {
    let (mut source, marks) = match inline {
        Inline::Text { text, marks } => (text.clone(), *marks),
        Inline::Link { text, marks, url } => (format!("[{text}]({})", url.as_str()), *marks),
        Inline::Mention { display, .. } => return format!("@{display}"),
    };
    if marks.code {
        source = format!("`{source}`");
    }
    if marks.italic {
        source = format!("_{source}_");
    }
    if marks.bold {
        source = format!("**{source}**");
    }
    source
}

/// Test fixtures write bodies as string literals; those are plain.
#[cfg(test)]
impl From<&str> for ComposedBody {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

#[cfg(test)]
impl From<String> for ComposedBody {
    fn from(text: String) -> Self {
        Self::plain(&text)
    }
}

fn with(marks: Marks, apply: impl FnOnce(&mut Marks)) -> Marks {
    let mut marks = marks;
    apply(&mut marks);
    marks
}

fn push_text(out: &mut Vec<Inline>, text: &str, marks: Marks) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text {
        text: previous,
        marks: previous_marks,
    }) = out.last_mut()
        && *previous_marks == marks
    {
        previous.push_str(text);
        return;
    }
    out.push(Inline::Text {
        text: text.into(),
        marks,
    });
}

fn parse_inline(input: &str, marks: Marks) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut literal_start = 0;
    let mut index = 0;
    while index < input.len() {
        if let Some((consumed, inlines)) = try_token(input, index, marks) {
            push_text(&mut out, &input[literal_start..index], marks);
            for inline in inlines {
                match inline {
                    Inline::Text { text, marks } => push_text(&mut out, &text, marks),
                    other => out.push(other),
                }
            }
            index += consumed;
            literal_start = index;
        } else {
            index += input[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    push_text(&mut out, &input[literal_start..], marks);
    out
}

fn has_edge_whitespace(content: &str) -> bool {
    content.starts_with(char::is_whitespace) || content.ends_with(char::is_whitespace)
}

/// Returns the bytes consumed and the produced inlines for a token that
/// starts at `index`, or `None` when the text there is literal.
fn try_token(input: &str, index: usize, marks: Marks) -> Option<(usize, Vec<Inline>)> {
    let rest = &input[index..];
    if rest.starts_with('`') && !marks.code {
        let close = rest[1..].find('`')?;
        let content = &rest[1..1 + close];
        if content.is_empty() {
            return None;
        }
        return Some((
            close + 2,
            vec![Inline::Text {
                text: content.into(),
                marks: with(marks, |m| m.code = true),
            }],
        ));
    }
    if rest.starts_with("**") && !marks.bold {
        let close = rest[2..].find("**")?;
        let content = &rest[2..2 + close];
        if content.is_empty() || has_edge_whitespace(content) {
            return None;
        }
        return Some((
            close + 4,
            parse_inline(content, with(marks, |m| m.bold = true)),
        ));
    }
    if rest.starts_with('_') && !marks.italic {
        // Only at a word boundary, so `snake_case_name` stays literal.
        let before = input[..index].chars().next_back();
        if before.is_some_and(char::is_alphanumeric) {
            return None;
        }
        let mut search = 1;
        while let Some(found) = rest[search..].find('_') {
            let close = search + found;
            let after = rest[close + 1..].chars().next();
            if !after.is_some_and(char::is_alphanumeric) {
                let content = &rest[1..close];
                if content.is_empty() || has_edge_whitespace(content) {
                    return None;
                }
                return Some((
                    close + 1,
                    parse_inline(content, with(marks, |m| m.italic = true)),
                ));
            }
            search = close + 1;
        }
        return None;
    }
    if rest.starts_with('[') {
        let text_end = rest.find(']')?;
        let text = &rest[1..text_end];
        if text.is_empty() || text.contains('[') {
            return None;
        }
        let target = rest[text_end + 1..].strip_prefix('(')?;
        let target_end = target.find(')')?;
        let url = SafeHttpsUrl::parse(&target[..target_end])?;
        return Some((
            text_end + 2 + target_end + 1,
            vec![Inline::Link {
                text: text.into(),
                marks,
                url,
            }],
        ));
    }
    None
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
    fn markdown_lite_is_total_on_arbitrary_input() {
        let alphabet = [
            '*', '_', '`', '[', ']', '(', ')', 'a', ' ', '\n', 'é', 'h', ':', '/',
        ];
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        for _ in 0..3000 {
            let mut source = String::new();
            for _ in 0..(seed % 24) {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                source.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            seed = seed.rotate_left(7) ^ 0x9E37;
            let body = ComposedBody::from_markdown_lite(&source);
            // Never loses visible characters other than the markers.
            assert!(body.plain_projection().len() <= source.len());
        }
        let _ = ComposedBody::from_markdown_lite(&"*".repeat(10_000));
        let _ = ComposedBody::from_markdown_lite(&"_a".repeat(10_000));
    }

    #[test]
    fn parses_each_mark_and_link() {
        let body =
            ComposedBody::from_markdown_lite("a **b** _c_ `d` [e](https://example.com/x?y=1&z=2)");
        assert_eq!(
            body.blocks()[0],
            vec![
                text("a "),
                marked("b", true, false, false),
                text(" "),
                marked("c", false, true, false),
                text(" "),
                marked("d", false, false, true),
                text(" "),
                Inline::Link {
                    text: "e".into(),
                    marks: Marks::default(),
                    url: SafeHttpsUrl::parse("https://example.com/x?y=1&z=2").unwrap(),
                },
            ]
        );
        assert!(!body.is_plain());
        assert_eq!(body.plain_projection(), "a b c d e");
    }

    #[test]
    fn marks_nest_and_code_is_literal() {
        let body = ComposedBody::from_markdown_lite("**a _b_** `**x**`");
        assert_eq!(
            body.blocks()[0],
            vec![
                marked("a ", true, false, false),
                marked("b", true, true, false),
                text(" "),
                marked("**x**", false, false, true),
            ]
        );
    }

    #[test]
    fn unbalanced_markers_stay_literal() {
        for source in [
            "**a",
            "a**",
            "_a",
            "a_",
            "`a",
            "[a](https://x",
            "[a]",
            "** a **",
            "__",
            "``",
            "[](https://x.y)",
            "[a](http://x.y)",
            "[a](javascript:alert(1))",
            "[a](https:/x.y)",
            "[a](https:///x)",
        ] {
            let body = ComposedBody::from_markdown_lite(source);
            assert!(body.is_plain(), "{source}");
            assert_eq!(body.plain_projection(), source, "{source}");
        }
    }

    #[test]
    fn snake_case_is_not_italic() {
        let body = ComposedBody::from_markdown_lite("use snake_case_names and _x_");
        assert_eq!(
            body.blocks()[0],
            vec![
                text("use snake_case_names and "),
                marked("x", false, true, false)
            ]
        );
    }

    #[test]
    fn plain_projection_round_trips_plain_drafts_exactly() {
        for source in [
            "hello",
            "a\n\nb",
            "  spaced  out ",
            "line\n",
            "é 日本 😀",
            "1 * 2 * 3",
        ] {
            for body in [
                ComposedBody::from_markdown_lite(source),
                ComposedBody::plain(source),
            ] {
                assert!(body.is_plain(), "{source:?}");
                assert_eq!(body.plain_projection(), source);
            }
        }
        // `plain` never interprets markers.
        let literal = ComposedBody::plain("**not bold**");
        assert!(literal.is_plain());
        assert_eq!(literal.plain_projection(), "**not bold**");
    }

    #[test]
    fn safe_urls_need_https_host_and_no_userinfo() {
        for good in [
            "https://example.com",
            "HTTPS://Example.com/a b".trim_end_matches(" b"),
        ] {
            assert!(SafeHttpsUrl::parse(good).is_some(), "{good}");
        }
        for bad in [
            "http://example.com",
            "https://user:pw@example.com",
            "https://@example.com",
            "javascript:alert(1)",
            "data:text/html,x",
            "https://exa mple.com",
            "https://example.com/\u{0007}",
            "",
        ] {
            assert!(SafeHttpsUrl::parse(bad).is_none(), "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_URL_BYTES));
        assert!(SafeHttpsUrl::parse(&long).is_none());
    }

    #[test]
    fn markdown_round_trip_property() {
        for source in [
            "plain",
            "**b** and _i_ and `c`",
            "**a**\n\n_b_",
            "[t](https://example.com/a?b=1)",
            "**_x_**",
            "**[t](https://example.com)** end",
            "**a****_b_**",
        ] {
            let body = ComposedBody::from_markdown_lite(source);
            let markdown = body.to_markdown_lite();
            assert_eq!(
                ComposedBody::from_markdown_lite(&markdown),
                body,
                "{source} -> {markdown}"
            );
        }
    }

    #[test]
    fn runs_rebuild_into_blocks_that_round_trip() {
        let runs = vec![
            marked("Hi", true, false, false),
            text("\nthere "),
            Inline::Link {
                text: "you".into(),
                marks: Marks::default(),
                url: SafeHttpsUrl::parse("https://example.com").unwrap(),
            },
        ];
        let body = ComposedBody::from_runs(&runs).expect("rebuilds");
        assert_eq!(body.blocks().len(), 2);
        assert_eq!(body.plain_projection(), "Hi\nthere you");
        assert_eq!(
            ComposedBody::from_markdown_lite(&body.to_markdown_lite()),
            body
        );
    }

    #[test]
    fn to_runs_and_from_runs_agree() {
        let body = ComposedBody::from_markdown_lite("**a** x\n\n_b_ [t](https://example.com)");
        assert_eq!(ComposedBody::from_runs(&body.to_runs()), Some(body));
    }

    #[test]
    fn lossy_runs_do_not_round_trip() {
        // A literal marker in received text would reparse as formatting, a
        // padded bold run cannot be written, and a link cannot span a line.
        for runs in [
            vec![text("2 ** 3 ** 4 a**b**")],
            vec![marked(" padded ", true, false, false)],
        ] {
            let body = ComposedBody::from_runs(&runs).expect("rebuilds");
            assert_ne!(
                ComposedBody::from_markdown_lite(&body.to_markdown_lite()),
                body,
                "{runs:?}"
            );
        }
        let spanning = vec![Inline::Link {
            text: "a\nb".into(),
            marks: Marks::default(),
            url: SafeHttpsUrl::parse("https://example.com").unwrap(),
        }];
        assert!(ComposedBody::from_runs(&spanning).is_none());
    }

    fn picked(name: &str, raw: u64) -> DraftMention {
        DraftMention {
            handle: MentionHandle::for_test(raw),
            display: name.into(),
        }
    }

    fn mention(name: &str, raw: u64) -> Inline {
        Inline::Mention {
            handle: MentionHandle::for_test(raw),
            display: name.into(),
        }
    }

    #[test]
    fn tokenizer_keeps_only_intact_mentions() {
        let people = [
            picked("Ada", 1),
            picked("Ada Lovelace", 2),
            picked("Bob", 3),
        ];
        let body = ComposedBody::compose("hi @Ada Lovelace and @Bob, also @Ada.", &people);
        assert_eq!(
            body.blocks()[0],
            vec![
                text("hi "),
                mention("Ada Lovelace", 2),
                text(" and "),
                mention("Bob", 3),
                text(", also "),
                mention("Ada", 1),
                text("."),
            ]
        );
        assert_eq!(body.mentions().len(), 3);
        assert_eq!(
            body.plain_projection(),
            "hi Ada Lovelace and Bob, also Ada."
        );
        assert!(!body.is_plain());
        // Not at a word start, running into more letters, edited, or never
        // picked: all stay plain text.
        for draft in ["mail a@Ada", "@Adam", "@Ad", "@ada", "plain @Eve", "Ada"] {
            let body = ComposedBody::compose(draft, &people[..1]);
            assert!(body.mentions().is_empty(), "{draft}");
            assert!(body.is_plain(), "{draft}");
            assert_eq!(body.plain_projection(), draft);
        }
        // The same member twice is two mentions; nothing picked means no change.
        let twice = ComposedBody::compose("@Bob @Bob", &people);
        assert_eq!(twice.mentions(), vec![people[2].handle, people[2].handle]);
        assert_eq!(
            ComposedBody::compose("@Bob", &[]),
            ComposedBody::from_markdown_lite("@Bob")
        );
    }

    #[test]
    fn mention_cap_20() {
        let people = [picked("Ada", 1)];
        let draft = "@Ada ".repeat(25);
        let body = ComposedBody::compose(draft.trim(), &people);
        assert_eq!(body.mentions().len(), MAX_MENTIONS_PER_MESSAGE);
        assert!(body.within_bounds());
        // The surplus stays literal text.
        assert!(
            body.plain_projection()
                .ends_with("@Ada @Ada @Ada @Ada @Ada")
        );
        let mut over = ComposedBody::compose("@Ada", &people);
        for _ in 0..MAX_MENTIONS_PER_MESSAGE {
            over.blocks[0].push(mention("Ada", 1));
        }
        assert!(!over.within_bounds());
    }

    #[test]
    fn marks_span_mentions() {
        let people = [picked("Bob", 3)];
        let body = ComposedBody::compose("**hi @Bob now**", &people);
        assert_eq!(
            body.blocks()[0],
            vec![
                marked("hi ", true, false, false),
                mention("Bob", 3),
                marked(" now", true, false, false),
            ]
        );
        let italic = ComposedBody::compose("_@Bob_", &people);
        assert_eq!(italic.blocks()[0], vec![mention("Bob", 3)]);
    }

    #[test]
    fn a_mention_inside_code_or_a_link_stays_literal() {
        let people = [picked("Bob", 3)];
        let code = ComposedBody::compose("`@Bob` and @Bob", &people);
        assert_eq!(
            code.blocks()[0],
            vec![
                marked("@Bob", false, false, true),
                text(" and "),
                mention("Bob", 3),
            ]
        );
        let link = ComposedBody::compose("[@Bob](https://example.com) @Bob", &people);
        assert_eq!(link.mentions().len(), 1);
        assert!(matches!(
            &link.blocks()[0][0],
            Inline::Link { text, .. } if text == "@Bob"
        ));
    }

    #[test]
    fn placeholder_in_draft_disables_mentions() {
        let people = [picked("Bob", 3)];
        let draft = format!("{MENTION_PLACEHOLDER} @Bob");
        let body = ComposedBody::compose(&draft, &people);
        assert!(body.mentions().is_empty());
        assert_eq!(body.plain_projection(), draft);
        // A link target that swallows the stand-in falls back safely.
        let tricky = ComposedBody::compose("[t](https://example.com/@Bob)", &people);
        assert!(tricky.mentions().len() <= 1);
        assert!(!tricky.plain_projection().contains(MENTION_PLACEHOLDER));
    }

    #[test]
    fn mentions_round_trip_through_runs_but_never_through_markdown() {
        let people = [picked("Bob", 3)];
        let body = ComposedBody::compose("hi @Bob\nbye", &people);
        assert_eq!(ComposedBody::from_runs(&body.to_runs()), Some(body));
    }

    #[test]
    fn bounds_are_enforced() {
        assert!(ComposedBody::plain("a\nb").within_bounds());
        assert!(!ComposedBody::plain(&"\n".repeat(MAX_BLOCKS)).within_bounds());
        let many = "_a_ ".repeat(MAX_INLINES);
        assert!(!ComposedBody::from_markdown_lite(&many).within_bounds());
    }
}
