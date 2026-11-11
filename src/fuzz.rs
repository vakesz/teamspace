//! Entry points for the `fuzz/` crate (cargo-fuzz). Compiled only with the
//! `fuzzing` feature and hidden from docs: every function takes arbitrary
//! bytes, must never panic, and asserts the invariant its production
//! counterpart promises.

use crate::integrations::cloud::GraphCloud;
use crate::integrations::{entra_auth, microsoft365_fuzz};

const CLOUDS: [&str; 4] = ["global", "usgov", "usgovdod", "china"];
/// Mirrors `MAX_PLAIN_MESSAGE_BYTES` in the chat adapter.
const MAX_MESSAGE_BODY_BYTES: usize = 8 * 1024;

/// Splits off the first byte as a selector, defaulting to zero.
fn split_selector(data: &[u8]) -> (u8, &[u8]) {
    match data.split_first() {
        Some((selector, rest)) => (*selector, rest),
        None => (0, data),
    }
}

/// Splits at the first newline: `(head, tail)`, with an empty tail when absent.
fn split_line(data: &[u8]) -> (&[u8], &[u8]) {
    match data.iter().position(|byte| *byte == b'\n') {
        Some(index) => (&data[..index], &data[index + 1..]),
        None => (data, &[]),
    }
}

/// A raw HTTP request against the OAuth loopback listener. The first line is
/// the expected `state`; the rest is the request.
pub fn loopback_callback(data: &[u8]) {
    let (state, request) = split_line(data);
    let state = String::from_utf8_lossy(state);
    entra_auth::fuzz_loopback_callback(request, &state, 53_682);
}

/// A Graph collection page body decoded by every adapter. The first byte picks
/// the adapter.
pub fn graph_page(data: &[u8]) {
    let (kind, body) = split_selector(data);
    microsoft365_fuzz::page(kind, body);
}

/// A Graph message body. The low bit of the first byte selects HTML. The result
/// is bounded, never empty, and free of control characters other than newline
/// and tab.
pub fn message_body(data: &[u8]) {
    let (selector, raw) = split_selector(data);
    let raw = String::from_utf8_lossy(raw);
    let text = microsoft365_fuzz::message_body(selector & 1 == 1, &raw);
    assert!(!text.is_empty(), "a body always renders something");
    assert!(text.len() <= MAX_MESSAGE_BODY_BYTES, "body is bounded");
    assert!(
        text.chars()
            .all(|c| !c.is_control() || matches!(c, '\n' | '\t')),
        "body has no control characters"
    );
}

/// `@odata.nextLink` validation. The first byte picks the cloud, the first line
/// is the expected path and the rest is the link. An accepted link must be
/// https on exactly that cloud's Graph host, with no port, credentials or
/// fragment, and exactly the expected path.
pub fn next_link(data: &[u8]) {
    let (selector, rest) = split_selector(data);
    let cloud = GraphCloud::parse(CLOUDS[usize::from(selector) % CLOUDS.len()])
        .expect("every listed cloud name parses");
    let (path, link) = split_line(rest);
    let (path, link) = (String::from_utf8_lossy(path), String::from_utf8_lossy(link));
    if let Some(url) = cloud.validated_next_link(&link, &path) {
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some(cloud.graph_host()));
        assert_eq!(url.port(), None);
        assert!(url.username().is_empty() && url.password().is_none());
        assert_eq!(url.fragment(), None);
        assert_eq!(url.path(), path);
    }
}

/// A persisted profile snapshot. The first byte picks the store; a decoded
/// snapshot must round-trip.
pub fn stored_snapshot(data: &[u8]) {
    let (kind, bytes) = split_selector(data);
    match kind % 8 {
        0 => crate::features::activity::fuzz_snapshot(bytes),
        1 => crate::features::calendar::fuzz_snapshot(bytes),
        2 => crate::features::calls::fuzz_snapshot(bytes),
        3 => crate::features::chat::fuzz_snapshot(bytes),
        4 => crate::features::files::fuzz_snapshot(bytes),
        5 => crate::features::teams::fuzz_snapshot(bytes),
        6 => crate::features::threads::fuzz_snapshot(bytes),
        _ => crate::app::presence::fuzz_snapshot(bytes),
    }
}
