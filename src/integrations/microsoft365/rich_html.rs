//! Wire rendering of a [`ComposedBody`] for Microsoft Graph chat messages.
//!
//! A body without formatting keeps the exact `text` form it always had. A
//! formatted body is emitted as an allow-listed HTML subset (`p`, `br`,
//! `strong`, `em`, `code`, `a`, and `at` for a mention): every text node and attribute value is
//! escaped, a link is emitted only after `SafeHttpsUrl` revalidation, and no
//! other tag or attribute can be produced.

use super::member_ids::MemberIdMap;
use crate::integrations::transport::{Inline, SafeHttpsUrl};
use crate::integrations::{ComposedBody, TransportError};

/// Adapter-side request bound, independent of the composer's own limit.
pub(super) const MAX_REMOTE_BODY_BYTES: usize = 28 * 1024;

/// One mention resolved for the request's `mentions` array. `id` pairs it
/// with the `at` element of the same number.
pub(super) struct WireMention {
    pub(super) id: usize,
    pub(super) display: String,
    pub(super) user_id: String,
}

/// A body ready to serialize as `{"contentType": .., "content": ..}`.
pub(super) struct WireBody {
    pub(super) content_type: &'static str,
    pub(super) content: String,
    pub(super) mentions: Vec<WireMention>,
}

/// Where mention handles may be resolved. A body that mentions anyone is
/// refused when there is no scope (an edit).
pub(super) enum MentionScope<'a> {
    None,
    Chat {
        directory: &'a MemberIdMap,
        chat_id: &'a str,
    },
}

/// Validates and renders `body`. `label` names the body in rejections. Every
/// mention handle must resolve in `scope`'s chat to a member whose remembered
/// name is the one the composer inserted; anything else is refused before any
/// request is made.
pub(super) fn wire_body(
    body: &ComposedBody,
    label: &'static str,
    scope: &MentionScope<'_>,
) -> Result<WireBody, TransportError> {
    wire_body_with_attachment(body, label, scope, None)
}

/// [`wire_body`], optionally ending in the marker of one file attachment
/// (`<attachment id="..."></attachment>`). With an attachment the body may be
/// empty and is always HTML. `attachment_id` must be a lowercase hyphenated
/// GUID; anything else is refused so no markup can be injected through it.
pub(super) fn wire_body_with_attachment(
    body: &ComposedBody,
    label: &'static str,
    scope: &MentionScope<'_>,
    attachment_id: Option<&str>,
) -> Result<WireBody, TransportError> {
    let projection = body.plain_projection();
    if let Some(id) = attachment_id
        && !is_attachment_guid(id)
    {
        return Err(TransportError::InvalidCommand(
            "the attachment reference is not valid".into(),
        ));
    }
    if projection.trim().is_empty() && attachment_id.is_none() {
        return Err(TransportError::InvalidCommand(format!(
            "{label} is required"
        )));
    }
    if projection.contains('\0') {
        return Err(TransportError::InvalidCommand(format!(
            "{label} contains a null byte"
        )));
    }
    if !body.within_bounds() {
        return Err(TransportError::InvalidCommand(format!(
            "{label} is too complex to send"
        )));
    }
    let mut mentions = Vec::new();
    for (id, (handle, display)) in body.mention_refs().into_iter().enumerate() {
        let MentionScope::Chat { directory, chat_id } = scope else {
            return Err(TransportError::InvalidCommand(format!(
                "{label} cannot contain mentions"
            )));
        };
        match directory.resolve(chat_id, handle) {
            Some((user_id, remembered)) if remembered == display => mentions.push(WireMention {
                id,
                display: display.to_owned(),
                user_id: user_id.to_owned(),
            }),
            _ => {
                return Err(TransportError::InvalidCommand(
                    "a mentioned person is not available in this chat; open People and mention them again"
                        .into(),
                ));
            }
        }
    }
    let wire = if let Some(id) = attachment_id {
        let mut content = if projection.trim().is_empty() {
            String::new()
        } else {
            render_html(body)
        };
        content.push_str(&format!("<attachment id=\"{id}\"></attachment>"));
        WireBody {
            content_type: "html",
            content,
            mentions,
        }
    } else if body.is_plain() {
        WireBody {
            content_type: "text",
            content: projection,
            mentions,
        }
    } else {
        WireBody {
            content_type: "html",
            content: render_html(body),
            mentions,
        }
    };
    if wire.content.len() > MAX_REMOTE_BODY_BYTES {
        return Err(TransportError::InvalidCommand(format!(
            "{label} exceeds the remote delivery limit"
        )));
    }
    Ok(wire)
}

/// A lowercase, hyphenated GUID (8-4-4-4-12 hex digits).
pub(super) fn is_attachment_guid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, length)| {
            group.len() == length
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

pub(super) fn render_html(body: &ComposedBody) -> String {
    let mut html = String::new();
    // Mentions are numbered in document order, matching `mentions` in the
    // request, which Graph pairs with each `at` element by this id.
    let mut mention_id = 0;
    for block in body.blocks() {
        html.push_str("<p>");
        if block.is_empty() {
            html.push_str("<br>");
        }
        for inline in block {
            render_inline(&mut html, inline, &mut mention_id);
        }
        html.push_str("</p>");
    }
    html
}

fn render_inline(html: &mut String, inline: &Inline, mention_id: &mut usize) {
    let (text, marks, link) = match inline {
        Inline::Text { text, marks } => (text, *marks, None),
        // Revalidate even though the type already proves it: this is the
        // last place an `href` can be produced.
        Inline::Link { text, marks, url } => (text, *marks, SafeHttpsUrl::parse(url.as_str())),
        Inline::Mention { display, .. } => {
            html.push_str(&format!("<at id=\"{mention_id}\">"));
            escape_into(html, display);
            html.push_str("</at>");
            *mention_id += 1;
            return;
        }
    };
    let tags = [
        (marks.bold, "strong"),
        (marks.italic, "em"),
        (marks.code, "code"),
    ];
    for (on, tag) in tags {
        if on {
            html.push('<');
            html.push_str(tag);
            html.push('>');
        }
    }
    if let Some(url) = &link {
        html.push_str("<a href=\"");
        escape_into(html, url.as_str());
        html.push_str("\">");
    }
    escape_into(html, text);
    if link.is_some() {
        html.push_str("</a>");
    }
    for (on, tag) in tags.into_iter().rev() {
        if on {
            html.push_str("</");
            html.push_str(tag);
            html.push('>');
        }
    }
}

fn escape_into(out: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(source: &str) -> String {
        render_html(&ComposedBody::from_markdown_lite(source))
    }

    #[test]
    fn exact_html_for_each_mark() {
        assert_eq!(html("**b**"), "<p><strong>b</strong></p>");
        assert_eq!(html("_i_"), "<p><em>i</em></p>");
        assert_eq!(html("`c`"), "<p><code>c</code></p>");
        assert_eq!(
            html("**a _b_**"),
            "<p><strong>a </strong><strong><em>b</em></strong></p>"
        );
        assert_eq!(
            html("x [t](https://example.com/p?a=1&b=2) y"),
            "<p>x <a href=\"https://example.com/p?a=1&amp;b=2\">t</a> y</p>"
        );
        assert_eq!(
            html("**a**\n\n_b_"),
            "<p><strong>a</strong></p><p><br></p><p><em>b</em></p>"
        );
    }

    #[test]
    fn only_a_lowercase_hyphenated_guid_can_become_an_attachment_id() {
        use super::is_attachment_guid;
        assert!(is_attachment_guid("153fa47d-18c9-4179-be08-9879815a9f90"));
        for bad in [
            "",
            "153FA47D-18C9-4179-BE08-9879815A9F90",
            "153fa47d18c94179be089879815a9f90",
            "153fa47d-18c9-4179-be08-9879815a9f9",
            "153fa47d-18c9-4179-be08-9879815a9f90\"><script>",
            "zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz",
            "153fa47d-18c9-4179-be08-9879815a9f90 ",
        ] {
            assert!(!is_attachment_guid(bad), "{bad:?}");
            let refused = super::wire_body_with_attachment(
                &ComposedBody::plain("x"),
                "m",
                &MentionScope::None,
                Some(bad),
            );
            assert!(
                refused.is_err(),
                "{bad:?} must be refused before any markup"
            );
        }
    }

    #[test]
    fn an_attachment_body_is_html_ends_in_the_marker_and_keeps_escaping() {
        let guid = "153fa47d-18c9-4179-be08-9879815a9f90";
        let with_text = super::wire_body_with_attachment(
            &ComposedBody::plain("a < b"),
            "m",
            &MentionScope::None,
            Some(guid),
        )
        .expect("text and a file");
        assert_eq!(with_text.content_type, "html");
        assert_eq!(
            with_text.content,
            format!("<p>a &lt; b</p><attachment id=\"{guid}\"></attachment>")
        );
        let file_only = super::wire_body_with_attachment(
            &ComposedBody::plain("  "),
            "m",
            &MentionScope::None,
            Some(guid),
        )
        .expect("a file alone");
        assert_eq!(
            file_only.content,
            format!("<attachment id=\"{guid}\"></attachment>")
        );
        // Without a file, an empty body is still refused.
        assert!(wire_body(&ComposedBody::plain("  "), "m", &MentionScope::None).is_err());
    }

    #[test]
    fn escapes_text_and_attribute_values() {
        assert_eq!(
            html("**<img src=x onerror=alert(1)> & \"q\" 'p'**"),
            "<p><strong>&lt;img src=x onerror=alert(1)&gt; &amp; &quot;q&quot; &#39;p&#39;</strong></p>"
        );
        let link = html("[t](https://example.com/\"onmouseover=\"x)");
        assert!(!link.contains("\"onmouseover"), "{link}");
    }

    #[test]
    fn http_javascript_and_data_links_become_text() {
        for source in [
            "**x** [t](http://example.com)",
            "**x** [t](javascript:alert(1))",
            "**x** [t](data:text/html;base64,AAAA)",
            "**x** [t](https://u:p@example.com)",
        ] {
            let rendered = html(source);
            assert!(!rendered.contains("<a"), "{rendered}");
            assert!(!rendered.contains("href"), "{rendered}");
        }
    }

    /// Every tag in the output must be one of the allow-listed forms.
    fn assert_only_allow_listed_tags(rendered: &str) {
        let mut rest = rendered;
        while let Some(index) = rest.find('<') {
            let tag_end = rest[index..].find('>').expect("tags are closed") + index;
            let tag = &rest[index + 1..tag_end];
            let allowed = matches!(
                tag,
                "p" | "/p" | "br" | "strong" | "/strong" | "em" | "/em" | "code" | "/code" | "/a"
            ) || (tag.starts_with("a href=\"https://")
                && tag.ends_with('"')
                && !tag[8..tag.len() - 1].contains('"'));
            assert!(allowed, "unexpected tag <{tag}> in {rendered}");
            rest = &rest[tag_end + 1..];
        }
    }

    #[test]
    fn never_emits_script_or_event_attributes() {
        for source in [
            "<script>alert(1)</script> **x**",
            "**<b onclick=1>x</b>**",
            "[<script>](https://example.com)",
            "**a** <img src=x onerror=alert(1)>",
            "_x_ [t](https://example.com/\" onmouseover=\"x)",
        ] {
            let rendered = html(source);
            assert_only_allow_listed_tags(&rendered);
            assert!(
                !rendered.to_ascii_lowercase().contains("<script"),
                "{rendered}"
            );
        }
    }

    #[test]
    fn plain_bodies_keep_the_text_wire_form() {
        let wire = wire_body(
            &ComposedBody::from_markdown_lite("a < b & c\n\nd"),
            "m",
            &MentionScope::None,
        )
        .unwrap();
        assert_eq!(
            (wire.content_type, wire.content.as_str()),
            ("text", "a < b & c\n\nd")
        );
        let wire = wire_body(
            &ComposedBody::from_markdown_lite("**a**"),
            "m",
            &MentionScope::None,
        )
        .unwrap();
        assert_eq!(wire.content_type, "html");
    }

    #[test]
    fn empty_null_and_oversize_bodies_are_rejected() {
        assert!(wire_body(&ComposedBody::plain("  \n "), "m", &MentionScope::None).is_err());
        assert!(wire_body(&ComposedBody::plain("a\0b"), "m", &MentionScope::None).is_err());
        assert!(
            wire_body(
                &ComposedBody::plain(&"a".repeat(MAX_REMOTE_BODY_BYTES + 1)),
                "m",
                &MentionScope::None
            )
            .is_err()
        );
        assert!(
            wire_body(
                &ComposedBody::plain(&"a".repeat(MAX_REMOTE_BODY_BYTES)),
                "m",
                &MentionScope::None
            )
            .is_ok()
        );
        // Escaping can push a body over the bound even when the source fits.
        let rich = format!("**{}**", "&".repeat(MAX_REMOTE_BODY_BYTES / 2));
        assert!(
            wire_body(
                &ComposedBody::from_markdown_lite(&rich),
                "m",
                &MentionScope::None
            )
            .is_err()
        );
    }
}
