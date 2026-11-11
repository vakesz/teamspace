//! Session-only state of the global search overlay's Microsoft 365 message
//! search.
//!
//! A search runs only when the user presses Enter on the overlay's
//! "Search Microsoft 365 messages" row (or its "more" row), never per
//! keystroke. Results are bound to the exact query and page that produced
//! them, are never persisted, and are cleared when the overlay closes, the
//! typed query changes, the session disconnects, or the profile changes.

use crate::integrations::{
    RemoteMessageSearchAvailability, RemoteMessageSearchProjection, RemoteSearchHit,
    RemoteSearchQuery,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteSearchPhase {
    Idle,
    /// A page is in flight (or queued) for `page`.
    Searching {
        page: u8,
    },
    Ready,
    /// Graph refused the search (for example a guest account).
    Denied,
    Failed,
}

#[derive(Debug)]
pub(crate) struct RemoteMessageSearchState {
    query: Option<RemoteSearchQuery>,
    phase: RemoteSearchPhase,
    hits: Vec<RemoteSearchHit>,
    /// The page the next "more" request would read.
    next_page: u8,
    more: bool,
    /// An un-drained request for the app shell to hand to the transport.
    request: Option<(RemoteSearchQuery, u8)>,
    cancellation_requested: bool,
}

impl Default for RemoteMessageSearchState {
    fn default() -> Self {
        Self {
            query: None,
            phase: RemoteSearchPhase::Idle,
            hits: Vec::new(),
            next_page: 0,
            more: false,
            request: None,
            cancellation_requested: false,
        }
    }
}

impl RemoteMessageSearchState {
    pub(crate) const fn phase(&self) -> RemoteSearchPhase {
        self.phase
    }

    pub(crate) fn hits(&self) -> &[RemoteSearchHit] {
        &self.hits
    }

    pub(crate) const fn has_more(&self) -> bool {
        self.more
    }

    pub(crate) fn query(&self) -> Option<&RemoteSearchQuery> {
        self.query.as_ref()
    }

    /// True once a search has been started and not yet cleared.
    pub(crate) const fn is_active(&self) -> bool {
        !matches!(self.phase, RemoteSearchPhase::Idle)
    }

    /// Starts a new search for `text`. A repeat of the search already in
    /// flight is ignored so a double Enter cannot restart it.
    pub(crate) fn start(&mut self, text: &str) -> bool {
        let Some(query) = RemoteSearchQuery::new(text) else {
            return false;
        };
        if matches!(self.phase, RemoteSearchPhase::Searching { .. })
            && self.query.as_ref() == Some(&query)
        {
            return false;
        }
        self.reset_results();
        self.request = Some((query.clone(), 0));
        self.query = Some(query);
        self.phase = RemoteSearchPhase::Searching { page: 0 };
        true
    }

    /// Requests the next page of the current search, keeping the hits shown.
    pub(crate) fn request_more(&mut self) -> bool {
        if self.phase != RemoteSearchPhase::Ready || !self.more {
            return false;
        }
        let Some(query) = self.query.clone() else {
            return false;
        };
        let page = self.next_page;
        self.request = Some((query, page));
        self.phase = RemoteSearchPhase::Searching { page };
        true
    }

    /// Applies one page. A result that does not answer the query and page now
    /// awaited (a late answer to an earlier query) is ignored.
    pub(crate) fn apply(&mut self, projection: RemoteMessageSearchProjection) -> bool {
        let RemoteSearchPhase::Searching { page } = self.phase else {
            return false;
        };
        if self.query.as_ref() != Some(projection.query()) || projection.page() != page {
            return false;
        }
        match projection.availability() {
            RemoteMessageSearchAvailability::Available => {
                self.more = projection.more();
                self.next_page = page.saturating_add(1);
                self.hits.extend(projection.into_hits());
                self.phase = RemoteSearchPhase::Ready;
            }
            RemoteMessageSearchAvailability::AccessDenied => {
                self.phase = RemoteSearchPhase::Denied;
            }
            RemoteMessageSearchAvailability::Unavailable => {
                self.phase = RemoteSearchPhase::Failed;
            }
        }
        true
    }

    /// Forgets the search. An in-flight request is asked to stop.
    pub(crate) fn clear(&mut self) {
        if matches!(self.phase, RemoteSearchPhase::Searching { .. }) || self.request.is_some() {
            self.cancellation_requested = true;
        }
        self.request = None;
        self.query = None;
        self.phase = RemoteSearchPhase::Idle;
        self.reset_results();
    }

    fn reset_results(&mut self) {
        self.hits.clear();
        self.more = false;
        self.next_page = 0;
    }

    pub(crate) fn take_request(&mut self) -> Option<(RemoteSearchQuery, u8)> {
        self.request.take()
    }

    pub(crate) fn take_cancellation_requested(&mut self) -> bool {
        std::mem::take(&mut self.cancellation_requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::{RemoteMessageSearchProjection, RemoteSearchQuery};

    fn page(text: &str, page: u8) -> RemoteMessageSearchProjection {
        RemoteMessageSearchProjection::unavailable(RemoteSearchQuery::new(text).unwrap(), page)
    }

    #[test]
    fn a_blank_query_never_starts_a_search() {
        let mut state = RemoteMessageSearchState::default();
        assert!(!state.start("   "));
        assert!(state.take_request().is_none());
        assert!(!state.is_active());
    }

    #[test]
    fn start_queues_exactly_one_request_and_ignores_a_repeat() {
        let mut state = RemoteMessageSearchState::default();
        assert!(state.start("budget"));
        assert!(!state.start("  budget "));
        let (query, page) = state.take_request().unwrap();
        assert_eq!(query, RemoteSearchQuery::new("budget").unwrap());
        assert_eq!(page, 0);
        assert!(state.take_request().is_none());
        assert_eq!(state.phase(), RemoteSearchPhase::Searching { page: 0 });
    }

    #[test]
    fn a_late_answer_to_another_query_or_page_is_ignored() {
        let mut state = RemoteMessageSearchState::default();
        state.start("budget");
        assert!(!state.apply(page("other", 0)));
        assert!(!state.apply(page("budget", 1)));
        assert_eq!(state.phase(), RemoteSearchPhase::Searching { page: 0 });
        assert!(state.apply(page("budget", 0)));
        assert_eq!(state.phase(), RemoteSearchPhase::Failed);
    }

    #[test]
    fn a_result_with_nothing_awaited_is_ignored() {
        let mut state = RemoteMessageSearchState::default();
        assert!(!state.apply(page("budget", 0)));
    }

    #[test]
    fn clearing_a_search_in_flight_requests_cancellation_once() {
        let mut state = RemoteMessageSearchState::default();
        state.start("budget");
        state.clear();
        assert!(state.take_cancellation_requested());
        assert!(!state.take_cancellation_requested());
        assert!(!state.is_active());
        // A late answer after clearing is dropped.
        assert!(!state.apply(page("budget", 0)));
    }

    #[test]
    fn clearing_an_idle_search_requests_no_cancellation() {
        let mut state = RemoteMessageSearchState::default();
        state.clear();
        assert!(!state.take_cancellation_requested());
    }

    #[test]
    fn a_new_search_replaces_the_old_one() {
        let mut state = RemoteMessageSearchState::default();
        state.start("one");
        let _ = state.take_request();
        assert!(state.start("two"));
        assert_eq!(
            state.take_request().unwrap().0,
            RemoteSearchQuery::new("two").unwrap()
        );
        // The late answer to the first query is ignored.
        assert!(!state.apply(page("one", 0)));
    }
}
