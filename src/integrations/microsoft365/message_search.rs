//! Microsoft Graph chat-message search.
//!
//! One bounded `POST /search/query` per explicit request (an Enter on the
//! global search overlay's Microsoft 365 row, or its "more" action), reading
//! one page of 25 hits for at most four pages. The query is a read-class POST
//! (`GraphSession::post_json_query`: one retry after a `401` renewal). Results
//! are normalized here: only opaque chat or channel targets, a sanitized
//! sender name, a display time, and an inert snippet cross the seam. The
//! sender's email address, the web link, the etag, and raw Graph objects are
//! dropped. See Graph's [search API](https://learn.microsoft.com/en-us/graph/api/search-query)
//! and its [Teams message search](https://learn.microsoft.com/en-us/graph/search-concept-chat-messages)
//! limits (no sorting, `total` is the page count only).

use super::chats::html_to_plain_text;
use super::mutation::is_dot_segment;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphJsonRequest, GraphSession, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    MAX_REMOTE_SEARCH_PAGES, ProviderResourceRef, REMOTE_SEARCH_PAGE_SIZE,
    RemoteMessageSearchProjection, RemoteSearchHit, RemoteSearchLocation, RemoteSearchQuery,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde_json::{Value, json};

const MAX_SEARCH_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_SNIPPET_BYTES: usize = 512;
const MAX_SENDER_BYTES: usize = 128;
const MAX_TIMESTAMP_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl SearchFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph search is temporarily unavailable".into(),
            ),
        }
    }
}

struct SearchPage {
    hits: Vec<RemoteSearchHit>,
    more: bool,
}

impl Microsoft365Transport {
    pub(super) fn poll_message_search(
        &mut self,
        query: RemoteSearchQuery,
        page: u8,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        if page >= MAX_REMOTE_SEARCH_PAGES {
            return Err(TransportError::InvalidCommand(
                "message search is limited to four pages".into(),
            ));
        }
        let completed = SyncInterest::MessageSearch {
            query: query.clone(),
            page,
        };
        match fetch_search_page(&self.session, &query, page, cancellation) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![completed],
                remote_message_search: Some(RemoteMessageSearchProjection::microsoft365(
                    query,
                    page,
                    result.hits,
                    result.more,
                )?),
                ..WorkspaceDelta::default()
            }),
            Err(SearchFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Guests and tenants without the grant cannot search Teams
                // messages. Say so quietly; nothing else is disturbed.
                completed: vec![completed],
                remote_message_search: Some(RemoteMessageSearchProjection::access_denied(query, page)),
                notices: vec![
                    "Microsoft 365 denied message search. Verify delegated Chat.Read access (guest accounts cannot search messages), then reconnect."
                        .into(),
                ],
                ..WorkspaceDelta::default()
            }),
            Err(SearchFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn check_cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), SearchFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(SearchFailure::Cancelled)
}

fn search_url(cloud: GraphCloud) -> Result<url::Url, SearchFailure> {
    cloud
        .graph_route(&["search", "query"], None)
        .map_err(|_| SearchFailure::Unavailable)
}

fn request_body(query: &RemoteSearchQuery, page: u8) -> Vec<u8> {
    let from = usize::from(page) * REMOTE_SEARCH_PAGE_SIZE;
    serde_json::to_vec(&json!({
        "requests": [{
            "entityTypes": ["chatMessage"],
            "query": { "queryString": query.as_str() },
            "from": from,
            "size": REMOTE_SEARCH_PAGE_SIZE,
        }]
    }))
    .unwrap_or_default()
}

fn fetch_search_page(
    session: &GraphSession<'_>,
    query: &RemoteSearchQuery,
    page: u8,
    cancellation: Option<&TransportCancellation>,
) -> Result<SearchPage, SearchFailure> {
    let url = search_url(session.cloud())?;
    let body = request_body(query, page);
    check_cancelled(cancellation)?;
    let response = session
        .post_json_query(
            &GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: &body,
            },
            MAX_SEARCH_RESPONSE_BYTES,
        )
        .map_err(|error| match error {
            GraphHttpError::Network
            | GraphHttpError::ResponseTooLarge
            | GraphHttpError::Unsupported
            | GraphHttpError::RenewedBeforeDelivery
            | GraphHttpError::RenewalUnavailable => SearchFailure::Unavailable,
        })?;
    // Never surface a stale query after cancellation.
    check_cancelled(cancellation)?;
    match response.status {
        200 => {}
        401 => return Err(SearchFailure::ReauthenticationRequired),
        403 => return Err(SearchFailure::AccessDenied),
        429 => return Err(SearchFailure::Throttled(response.retry_after_seconds)),
        _ => return Err(SearchFailure::Unavailable),
    }
    let parsed =
        serde_json::from_slice::<Value>(&response.body).map_err(|_| SearchFailure::Unavailable)?;
    let container = parsed
        .get("value")
        .and_then(Value::as_array)
        .and_then(|responses| responses.first())
        .and_then(|response| response.get("hitsContainers"))
        .and_then(Value::as_array)
        .and_then(|containers| containers.first());
    let Some(container) = container else {
        return Ok(SearchPage {
            hits: Vec::new(),
            more: false,
        });
    };
    let rows = container
        .get("hits")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let more = container
        .get("moreResultsAvailable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || rows.len() > REMOTE_SEARCH_PAGE_SIZE;
    let hits = rows
        .iter()
        .take(REMOTE_SEARCH_PAGE_SIZE)
        .filter_map(normalize_hit)
        .collect();
    check_cancelled(cancellation)?;
    Ok(SearchPage { hits, more })
}

/// Lenient per-hit normalization: anything that is not exactly a chatMessage
/// with a usable chat or channel target is skipped and never poisons a page.
fn normalize_hit(hit: &Value) -> Option<RemoteSearchHit> {
    let resource = hit.get("resource")?.as_object()?;
    let odata_type = resource.get("@odata.type")?.as_str()?;
    if odata_type.trim_start_matches('#') != "microsoft.graph.chatMessage" {
        return None;
    }
    let identifier = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(normalized_identifier)
            .filter(|id| !is_dot_segment(id))
    };
    let message = identifier(resource.get("id")).map(ProviderResourceRef::microsoft365);
    let location = if let Some(chat) = identifier(resource.get("chatId")) {
        RemoteSearchLocation::Chat {
            chat: ProviderResourceRef::microsoft365(chat),
            message,
        }
    } else {
        let channel = resource.get("channelIdentity")?.as_object()?;
        let team = identifier(channel.get("teamId"))?;
        let channel = identifier(channel.get("channelId"))?;
        RemoteSearchLocation::Channel {
            team: ProviderResourceRef::microsoft365(team),
            channel: ProviderResourceRef::microsoft365(channel),
        }
    };
    // Only the display name of the sender survives: the address is dropped.
    let sender = resource
        .get("from")
        .and_then(|from| {
            from.pointer("/emailAddress/name")
                .or_else(|| from.pointer("/user/displayName"))
        })
        .and_then(Value::as_str)
        .map(|name| safe_display(name, MAX_SENDER_BYTES))
        .unwrap_or_default();
    let sent_at = resource
        .get("createdDateTime")
        .and_then(Value::as_str)
        .map(|value| safe_display(value, MAX_TIMESTAMP_BYTES))
        .unwrap_or_default();
    // The summary carries highlight markup (`<c0>match</c0>`); reduce it to
    // inert text.
    let snippet = hit
        .get("summary")
        .and_then(Value::as_str)
        .map(|summary| safe_display(&html_to_plain_text(summary), MAX_SNIPPET_BYTES))
        .unwrap_or_default();
    Some(RemoteSearchHit::microsoft365(
        location, sender, sent_at, snippet,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::microsoft365::{
        GraphHttpClient, GraphHttpResponse, Microsoft365Transport,
    };
    use crate::integrations::{AccountIdentity, ConnectionStatus, WorkspaceTransport};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    #[derive(Default)]
    struct State {
        posts: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        urls: Mutex<Vec<String>>,
        bodies: Mutex<Vec<String>>,
        limits: Mutex<Vec<u64>>,
        cancel_on_post: Mutex<Option<TransportCancellation>>,
    }

    struct Client(Arc<State>);

    impl GraphHttpClient for Client {
        fn get_me(&self, _: &Url, _: &str) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn post_json_query(
            &self,
            request: GraphJsonRequest<'_>,
            _: &str,
            limit: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .urls
                .lock()
                .unwrap()
                .push(request.url.as_str().into());
            self.0
                .bodies
                .lock()
                .unwrap()
                .push(String::from_utf8(request.body.to_vec()).unwrap());
            self.0.limits.lock().unwrap().push(limit);
            if let Some(cancel) = self.0.cancel_on_post.lock().unwrap().as_ref() {
                cancel.cancel();
            }
            self.0
                .posts
                .lock()
                .unwrap()
                .pop_front()
                .expect("post scripted")
        }
    }

    fn resp(status: u16, body: Value) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.to_string().into_bytes(),
        })
    }

    fn transport(state: &Arc<State>, cloud: GraphCloud) -> Microsoft365Transport {
        let mut transport = Microsoft365Transport::with_client_in(
            cloud,
            "sensitive-access-token",
            Box::new(Client(Arc::clone(state))),
        );
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        transport
    }

    fn query() -> RemoteSearchQuery {
        RemoteSearchQuery::new("budget review").unwrap()
    }

    fn search(
        transport: &mut Microsoft365Transport,
        page: u8,
    ) -> Result<WorkspaceDelta, TransportError> {
        transport.poll(SyncInterest::MessageSearch {
            query: query(),
            page,
        })
    }

    fn container(hits: Vec<Value>, more: bool) -> Value {
        json!({"value": [{"searchTerms": ["budget"], "hitsContainers": [
            {"hits": hits, "total": 1, "moreResultsAvailable": more}]}]})
    }

    fn chat_hit(id: &str, chat: &str) -> Value {
        json!({
            "hitId": "outlook-id", "rank": 1,
            "summary": "...the <c0>budget</c0> &amp; plan",
            "resource": {
                "@odata.type": "microsoft.graph.chatMessage",
                "id": id, "chatId": chat,
                "createdDateTime": "2022-07-14T07:01:01Z",
                "webLink": "https://outlook.office365.com/owa/?ItemID=SECRETLINK",
                "etag": "ETAGVALUE",
                "from": {"emailAddress": {"name": "Goncalo", "address": "gtorres@contoso.com"}},
                "channelIdentity": {}
            }
        })
    }

    #[test]
    fn body_is_a_single_chatmessage_request_of_25_with_a_page_offset() {
        let state = Arc::new(State::default());
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, container(vec![], false)));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, container(vec![], false)));
        let mut t = transport(&state, GraphCloud::Global);
        search(&mut t, 0).unwrap();
        search(&mut t, 2).unwrap();
        let first: Value = serde_json::from_str(&state.bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(
            first,
            json!({"requests": [{"entityTypes": ["chatMessage"],
                "query": {"queryString": "budget review"}, "from": 0, "size": 25}]})
        );
        let third: Value = serde_json::from_str(&state.bodies.lock().unwrap()[1]).unwrap();
        assert_eq!(third["requests"][0]["from"], 50);
        assert!(state.urls.lock().unwrap()[0].ends_with("/v1.0/search/query"));
        assert_eq!(state.limits.lock().unwrap()[0], MAX_SEARCH_RESPONSE_BYTES);
    }

    #[test]
    fn every_cloud_uses_its_own_graph_host() {
        for cloud in GraphCloud::ALL {
            let state = Arc::new(State::default());
            state
                .posts
                .lock()
                .unwrap()
                .push_back(resp(200, container(vec![], false)));
            let mut t = transport(&state, cloud);
            search(&mut t, 0).unwrap();
            let url = Url::parse(&state.urls.lock().unwrap()[0]).unwrap();
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
        }
    }

    #[test]
    fn a_page_past_the_fourth_is_refused_before_any_request() {
        let state = Arc::new(State::default());
        let mut t = transport(&state, GraphCloud::Global);
        assert!(matches!(
            search(&mut t, 4),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.bodies.lock().unwrap().is_empty());
    }

    #[test]
    fn hits_become_targets_and_never_carry_email_weblink_or_etag() {
        let state = Arc::new(State::default());
        state.posts.lock().unwrap().push_back(resp(
            200,
            container(vec![chat_hit("1657782060227", "19:chat@thread.v2")], true),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        assert!(projection.more());
        let hit = &projection.hits()[0];
        assert_eq!(hit.sender(), "Goncalo");
        assert_eq!(hit.sent_at(), "2022-07-14T07:01:01Z");
        assert!(matches!(
            hit.location(),
            RemoteSearchLocation::Chat {
                message: Some(_),
                ..
            }
        ));
        let debug = format!("{projection:?}");
        for forbidden in [
            "gtorres",
            "SECRETLINK",
            "ETAGVALUE",
            "outlook-id",
            "19:chat",
        ] {
            assert!(!debug.contains(forbidden), "{forbidden} leaked");
        }
    }

    #[test]
    fn highlight_markup_is_stripped_from_the_snippet() {
        let state = Arc::new(State::default());
        state.posts.lock().unwrap().push_back(resp(
            200,
            container(vec![chat_hit("1", "19:chat@thread.v2")], false),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        let snippet = projection.hits()[0].snippet();
        assert!(!snippet.contains('<') && !snippet.contains("c0"));
        assert!(snippet.contains("budget"));
    }

    #[test]
    fn a_channel_hit_carries_team_and_channel_targets() {
        let state = Arc::new(State::default());
        let hit = json!({"summary": "hi", "resource": {
            "@odata.type": "#microsoft.graph.chatMessage", "id": "9",
            "channelIdentity": {"teamId": "team-1", "channelId": "19:chan@thread.tacv2"}}});
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, container(vec![hit], false)));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        assert!(matches!(
            projection.hits()[0].location(),
            RemoteSearchLocation::Channel { .. }
        ));
    }

    #[test]
    fn parsing_is_lenient_and_strict_about_the_odata_type() {
        let state = Arc::new(State::default());
        let wrong_type =
            json!({"resource": {"@odata.type": "microsoft.graph.message", "chatId": "19:x"}});
        let no_target =
            json!({"resource": {"@odata.type": "microsoft.graph.chatMessage", "id": "1"}});
        let dot_chat =
            json!({"resource": {"@odata.type": "microsoft.graph.chatMessage", "chatId": ".."}});
        let not_object = json!("nope");
        let good = chat_hit("1", "19:chat@thread.v2");
        state.posts.lock().unwrap().push_back(resp(
            200,
            container(
                vec![wrong_type, no_target, dot_chat, not_object, good],
                false,
            ),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        assert_eq!(projection.hits().len(), 1);
    }

    #[test]
    fn a_missing_container_is_an_empty_page() {
        let state = Arc::new(State::default());
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": []})));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        assert!(projection.hits().is_empty() && !projection.more());
    }

    #[test]
    fn a_page_is_capped_at_25_hits() {
        let state = Arc::new(State::default());
        let hits = (0..40)
            .map(|i| chat_hit(&i.to_string(), "19:c@thread.v2"))
            .collect();
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, container(hits, false)));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = search(&mut t, 0).unwrap().remote_message_search.unwrap();
        assert_eq!(projection.hits().len(), 25);
        assert!(projection.more());
    }

    #[test]
    fn an_oversized_response_is_unavailable() {
        let state = Arc::new(State::default());
        state
            .posts
            .lock()
            .unwrap()
            .push_back(Err(GraphHttpError::ResponseTooLarge));
        let mut t = transport(&state, GraphCloud::Global);
        assert!(matches!(
            search(&mut t, 0),
            Err(TransportError::Unavailable(_))
        ));
    }

    #[test]
    fn statuses_map_to_denied_throttled_reauth_and_unavailable() {
        let denied = Arc::new(State::default());
        denied.posts.lock().unwrap().push_back(resp(403, json!({})));
        let mut t = transport(&denied, GraphCloud::Global);
        let delta = search(&mut t, 0).unwrap();
        assert_eq!(
            delta.remote_message_search.unwrap().availability(),
            crate::integrations::RemoteMessageSearchAvailability::AccessDenied
        );
        assert!(!delta.notices.is_empty());

        let throttled = Arc::new(State::default());
        throttled
            .posts
            .lock()
            .unwrap()
            .push_back(Ok(GraphHttpResponse {
                status: 429,
                retry_after_seconds: Some(7),
                body: Vec::new(),
            }));
        let mut t = transport(&throttled, GraphCloud::Global);
        assert_eq!(
            search(&mut t, 0).unwrap_err(),
            TransportError::Throttled {
                retry_after_seconds: Some(7)
            }
        );

        let reauth = Arc::new(State::default());
        reauth.posts.lock().unwrap().push_back(resp(401, json!({})));
        // The session renews once on a 401 only when it can; without a
        // refresher the 401 is reported.
        let mut t = transport(&reauth, GraphCloud::Global);
        assert_eq!(
            search(&mut t, 0).unwrap_err(),
            TransportError::ReauthenticationRequired
        );

        let server = Arc::new(State::default());
        server.posts.lock().unwrap().push_back(resp(500, json!({})));
        let mut t = transport(&server, GraphCloud::Global);
        assert!(matches!(
            search(&mut t, 0),
            Err(TransportError::Unavailable(_))
        ));
    }

    #[test]
    fn cancellation_discards_the_response() {
        let state = Arc::new(State::default());
        let cancellation = TransportCancellation::default();
        *state.cancel_on_post.lock().unwrap() = Some(cancellation.clone());
        state.posts.lock().unwrap().push_back(resp(
            200,
            container(vec![chat_hit("1", "19:c@thread.v2")], false),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let result = t.poll_cancellable(
            SyncInterest::MessageSearch {
                query: query(),
                page: 0,
            },
            &cancellation,
        );
        assert_eq!(result.unwrap_err(), TransportError::Cancelled);
    }

    #[test]
    fn the_query_text_never_reaches_a_debug_string_of_the_interest() {
        let interest = SyncInterest::MessageSearch {
            query: query(),
            page: 1,
        };
        assert!(!format!("{interest:?}").contains("budget"));
    }
}
