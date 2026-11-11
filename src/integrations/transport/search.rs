//! Microsoft 365 chat-message search, as it crosses the integration seam.
//!
//! A search is a user-typed query (never persisted) and a bounded page of
//! normalized hits. A hit carries only opaque targets, a sanitized sender
//! name, a display time, and an inert snippet: no email address, web link,
//! etag, or raw Graph object ever leaves the adapter.

use super::{ProviderResourceRef, RemoteMessageInstant};
use crate::integrations::TransportError;
use std::fmt;

/// The longest query, in characters, that may be sent to Graph.
const MAX_REMOTE_SEARCH_QUERY_CHARS: usize = 256;

/// The number of result pages one query may read (`from` offsets of
/// `page * RESULTS_PER_PAGE`).
pub(in crate::integrations) const MAX_REMOTE_SEARCH_PAGES: u8 = 4;

/// Hits requested and retained per page.
pub(in crate::integrations) const REMOTE_SEARCH_PAGE_SIZE: usize = 25;

/// A validated search query. The text can be private (a person's name, a
/// project), so `Debug` reveals only its length.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct RemoteSearchQuery(String);

impl RemoteSearchQuery {
    /// Trims `text` and accepts one to [`MAX_REMOTE_SEARCH_QUERY_CHARS`]
    /// characters with no control character.
    pub(crate) fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        let length = text.chars().count();
        ((1..=MAX_REMOTE_SEARCH_QUERY_CHARS).contains(&length)
            && !text.chars().any(char::is_control))
        .then(|| Self(text.to_owned()))
    }

    pub(in crate::integrations) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RemoteSearchQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSearchQuery")
            .field("chars", &self.0.chars().count())
            .finish()
    }
}

/// Where a hit lives. Both forms are opaque, adapter-issued targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RemoteSearchLocation {
    Chat {
        chat: ProviderResourceRef,
        message: Option<ProviderResourceRef>,
    },
    Channel {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
}

/// One normalized hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteSearchHit {
    location: RemoteSearchLocation,
    sender: String,
    sent_at: String,
    /// Parsed once at the seam; `None` when the provider time is unknown.
    sent_instant: Option<RemoteMessageInstant>,
    snippet: String,
}

impl RemoteSearchHit {
    pub(in crate::integrations) fn microsoft365(
        location: RemoteSearchLocation,
        sender: String,
        sent_at: String,
        snippet: String,
    ) -> Self {
        Self {
            location,
            sender,
            sent_instant: RemoteMessageInstant::parse(&sent_at),
            sent_at,
            snippet,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        location: RemoteSearchLocation,
        sender: &str,
        sent_at: &str,
        snippet: &str,
    ) -> Self {
        Self::microsoft365(location, sender.into(), sent_at.into(), snippet.into())
    }

    pub(crate) const fn location(&self) -> &RemoteSearchLocation {
        &self.location
    }

    pub(crate) fn sender(&self) -> &str {
        &self.sender
    }

    pub(crate) fn sent_at(&self) -> &str {
        &self.sent_at
    }

    pub(crate) const fn sent_instant(&self) -> Option<RemoteMessageInstant> {
        self.sent_instant
    }

    pub(crate) fn snippet(&self) -> &str {
        &self.snippet
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteMessageSearchAvailability {
    Available,
    /// Graph refused the search (for example a guest account, or a missing
    /// `Chat.Read` grant).
    AccessDenied,
    /// A transient failure; carries no hits.
    Unavailable,
}

/// One page of hits for one query. It is session-only and bound to the exact
/// query and page it answers, so a stale result is recognisable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteMessageSearchProjection {
    query: RemoteSearchQuery,
    page: u8,
    availability: RemoteMessageSearchAvailability,
    hits: Vec<RemoteSearchHit>,
    more: bool,
}

impl RemoteMessageSearchProjection {
    pub(in crate::integrations) fn microsoft365(
        query: RemoteSearchQuery,
        page: u8,
        hits: Vec<RemoteSearchHit>,
        more: bool,
    ) -> Result<Self, TransportError> {
        if page >= MAX_REMOTE_SEARCH_PAGES || hits.len() > REMOTE_SEARCH_PAGE_SIZE {
            return Err(TransportError::Unavailable(
                "Microsoft Graph returned an unexpected search page".into(),
            ));
        }
        Ok(Self {
            query,
            page,
            availability: RemoteMessageSearchAvailability::Available,
            hits,
            // Never offer a page past the bound.
            more: more && page + 1 < MAX_REMOTE_SEARCH_PAGES,
        })
    }

    #[cfg(test)]
    pub(crate) fn available_for_test(
        query: RemoteSearchQuery,
        page: u8,
        hits: Vec<RemoteSearchHit>,
        more: bool,
    ) -> Self {
        Self::microsoft365(query, page, hits, more).expect("a bounded test page")
    }

    pub(in crate::integrations) fn access_denied(query: RemoteSearchQuery, page: u8) -> Self {
        Self::without_hits(query, page, RemoteMessageSearchAvailability::AccessDenied)
    }

    /// The row-free marker the app transport layer emits for a transient
    /// failure, so a Searching state cannot stay stuck.
    pub(crate) fn unavailable(query: RemoteSearchQuery, page: u8) -> Self {
        Self::without_hits(query, page, RemoteMessageSearchAvailability::Unavailable)
    }

    fn without_hits(
        query: RemoteSearchQuery,
        page: u8,
        availability: RemoteMessageSearchAvailability,
    ) -> Self {
        Self {
            query,
            page,
            availability,
            hits: Vec::new(),
            more: false,
        }
    }

    pub(crate) const fn query(&self) -> &RemoteSearchQuery {
        &self.query
    }

    pub(crate) const fn page(&self) -> u8 {
        self.page
    }

    pub(crate) const fn availability(&self) -> RemoteMessageSearchAvailability {
        self.availability
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> &[RemoteSearchHit] {
        &self.hits
    }

    pub(crate) fn into_hits(self) -> Vec<RemoteSearchHit> {
        self.hits
    }

    pub(crate) const fn more(&self) -> bool {
        self.more
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_trimmed_bounded_and_control_free() {
        assert_eq!(
            RemoteSearchQuery::new("  budget  ").unwrap().as_str(),
            "budget"
        );
        assert!(RemoteSearchQuery::new("   ").is_none());
        assert!(RemoteSearchQuery::new("a\nb").is_none());
        assert!(RemoteSearchQuery::new(&"x".repeat(256)).is_some());
        assert!(RemoteSearchQuery::new(&"x".repeat(257)).is_none());
        // The bound is in characters, not bytes.
        assert!(RemoteSearchQuery::new(&"é".repeat(256)).is_some());
    }

    #[test]
    fn query_debug_prints_only_its_length() {
        let query = RemoteSearchQuery::new("secret project").unwrap();
        let debug = format!("{query:?}");
        assert!(debug.contains("14"));
        assert!(!debug.contains("secret"));
        let projection = RemoteMessageSearchProjection::unavailable(query, 0);
        assert!(!format!("{projection:?}").contains("secret"));
    }

    #[test]
    fn a_page_past_the_bound_never_offers_more() {
        let query = RemoteSearchQuery::new("x").unwrap();
        let last = RemoteMessageSearchProjection::microsoft365(
            query.clone(),
            MAX_REMOTE_SEARCH_PAGES - 1,
            Vec::new(),
            true,
        )
        .unwrap();
        assert!(!last.more());
        assert!(
            RemoteMessageSearchProjection::microsoft365(
                query,
                MAX_REMOTE_SEARCH_PAGES,
                Vec::new(),
                false
            )
            .is_err()
        );
    }
}
