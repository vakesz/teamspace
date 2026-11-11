//! Decides whether a Graph chat message may be edited.
//!
//! An edit replaces the whole body, so it is offered only when that cannot
//! discard anything: the row is the signed-in user's own, not deleted, an
//! ordinary message, free of attachments, mentions and policy tags, and its
//! body is text or HTML limited to the formatting this client can compose
//! (paragraphs, bold, italic, code, and `https` links).

use super::rich_runs::{MAX_RICH_TAG_BYTES, html_to_rich_runs, is_safe_link_tag};
use super::{
    GraphChatMessage, MAX_PLAIN_MESSAGE_BYTES, MAX_RAW_MESSAGE_BODY_BYTES, bounded_ascii_delimiter,
};

/// `own` and `has_target` come from the already-normalized row.
pub(super) fn is_plain_text_editable(
    message: &GraphChatMessage,
    own: bool,
    deleted: bool,
    has_target: bool,
) -> bool {
    if !own || deleted || !has_target {
        return false;
    }
    if message.message_type.as_deref() != Some("message")
        || message.attachments.any
        || message.mentions.any
        || message.policy_violation.is_some()
    {
        return false;
    }
    let Some(body) = message.body.as_ref() else {
        return false;
    };
    let Some(content) = body
        .content
        .as_deref()
        .filter(|content| !content.is_empty())
    else {
        return false;
    };
    match body.content_type.as_deref() {
        Some(kind) if kind.eq_ignore_ascii_case("html") => {
            // A longer body is truncated in the plain-text projection, so
            // saving it would silently drop the tail.
            content.len() <= MAX_RAW_MESSAGE_BODY_BYTES && html_is_editable_markup(content)
        }
        Some(kind) if kind.eq_ignore_ascii_case("text") => content.len() <= MAX_PLAIN_MESSAGE_BYTES,
        _ => false,
    }
}

/// Only `p`, `br`, `div`, `b`, `strong`, `i`, `em`, `code` and `pre` without
/// attributes, and `a` whose only attribute is a valid `https` `href`, may
/// appear; entities are text. Any other tag, such as an image, mention,
/// table or span, fails closed. Formatted markup must also parse into runs,
/// so a malformed body is never rewritten from a lossy reading.
fn html_is_editable_markup(raw: &str) -> bool {
    let mut index = 0;
    let mut formatted = false;
    while index < raw.len() {
        if raw.as_bytes()[index] != b'<' {
            index += 1;
            continue;
        }
        let Some(close) = bounded_ascii_delimiter(raw, index + 1, b'>', MAX_RICH_TAG_BYTES) else {
            return false;
        };
        let inner = raw[index + 1..close].trim();
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_start();
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        let attributes = body[name_end..].trim().trim_end_matches('/').trim();
        match name.as_str() {
            "p" | "br" | "div" if attributes.is_empty() => {}
            "b" | "strong" | "i" | "em" | "code" | "pre" if attributes.is_empty() => {
                formatted = true;
            }
            "a" if closing && attributes.is_empty() => formatted = true,
            "a" if !closing && is_safe_link_tag(attributes) => formatted = true,
            _ => return false,
        }
        index = close + 1;
    }
    !formatted || html_to_rich_runs(raw).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(json: serde_json::Value) -> GraphChatMessage {
        serde_json::from_value(json).expect("fixture must parse")
    }

    fn base() -> serde_json::Value {
        serde_json::json!({
            "id": "m1",
            "messageType": "message",
            "body": {"contentType": "text", "content": "hello"},
        })
    }

    fn with(key: &str, value: serde_json::Value) -> GraphChatMessage {
        let mut json = base();
        json[key] = value;
        message(json)
    }

    fn editable(message: &GraphChatMessage) -> bool {
        is_plain_text_editable(message, true, false, true)
    }

    #[test]
    fn a_plain_own_message_is_editable() {
        assert!(editable(&message(base())));
        assert!(editable(&with("attachments", serde_json::json!([]))));
        assert!(editable(&with("mentions", serde_json::Value::Null)));
        assert!(editable(&with("policyViolation", serde_json::Value::Null)));
    }

    #[test]
    fn ownership_deletion_and_target_are_required() {
        let plain = message(base());
        assert!(!is_plain_text_editable(&plain, false, false, true));
        assert!(!is_plain_text_editable(&plain, true, true, true));
        assert!(!is_plain_text_editable(&plain, true, false, false));
    }

    #[test]
    fn only_ordinary_messages_are_editable() {
        for kind in [
            serde_json::json!("systemEventMessage"),
            serde_json::json!("Message"),
            serde_json::Value::Null,
            serde_json::json!(5),
        ] {
            assert!(!editable(&with("messageType", kind.clone())), "{kind}");
        }
        let mut json = base();
        json.as_object_mut().expect("object").remove("messageType");
        assert!(!editable(&message(json)));
    }

    #[test]
    fn any_attachment_mention_or_policy_metadata_blocks_editing() {
        assert!(!editable(&with(
            "attachments",
            serde_json::json!([{"contentType": "reference", "contentUrl": "https://x"}])
        )));
        // Wrong-shape metadata the lenient parser discards must still block.
        assert!(!editable(&with("attachments", serde_json::json!([1]))));
        assert!(!editable(&with("attachments", serde_json::json!("x"))));
        assert!(!editable(&with(
            "mentions",
            serde_json::json!([{"id": 0, "mentionText": "Ada"}])
        )));
        assert!(!editable(&with("mentions", serde_json::json!(["bad"]))));
        assert!(!editable(&with(
            "policyViolation",
            serde_json::json!({"verdictDetails": "allowFalsePositiveOverride"})
        )));
    }

    #[test]
    fn html_is_editable_only_with_composable_formatting() {
        let html = |content: &str| {
            with(
                "body",
                serde_json::json!({"contentType": "html", "content": content}),
            )
        };
        for ok in [
            "<p>hello</p>",
            "<div>a<br>b<br/></div><P>c &amp; d</P>",
            "plain &lt;text&gt;",
            "<p>a <strong>b</strong> <em>c</em> <code>d</code></p>",
            "<p>see <a href=\"https://example.com/x?a=1&amp;b=2\">link</a></p>",
            "<pre>code\nblock</pre><b>x</b>",
        ] {
            assert!(editable(&html(ok)), "{ok}");
        }
        for bad in [
            "<p>see <a href=\"http://example.com\">link</a></p>",
            "<p>see <a href=\"https://example.com\" onclick=\"x\">link</a></p>",
            "<p>see <a href=\"javascript:alert(1)\">link</a></p>",
            "<b>unclosed",
            "<b onclick=x>x</b>",
            "<img src=x>",
            "<at id=\"0\">Ada</at>",
            "<span>x</span>",
            "<p style=\"x\">x</p>",
            "<table></table>",
            "<p>unterminated <",
            "<attachment id=\"1\"></attachment>",
        ] {
            assert!(!editable(&html(bad)), "{bad}");
        }
    }

    #[test]
    fn empty_missing_unknown_or_oversized_bodies_are_not_editable() {
        for body in [
            serde_json::json!({"contentType": "text", "content": ""}),
            serde_json::json!({"contentType": "text"}),
            serde_json::json!({"contentType": "markdown", "content": "x"}),
            serde_json::json!({"content": "x"}),
            serde_json::json!({
                "contentType": "text",
                "content": "x".repeat(MAX_PLAIN_MESSAGE_BYTES + 1)
            }),
            serde_json::json!({
                "contentType": "html",
                "content": "x".repeat(MAX_RAW_MESSAGE_BODY_BYTES + 1)
            }),
        ] {
            assert!(!editable(&with("body", body.clone())), "{body}");
        }
        let mut json = base();
        json.as_object_mut().expect("object").remove("body");
        assert!(!editable(&message(json)));
    }
}
