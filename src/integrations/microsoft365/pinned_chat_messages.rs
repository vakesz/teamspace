//! Microsoft Graph selected-chat pinned-message normalization.
//!
//! This module owns the one bounded `pinnedMessages?$expand=message` read for
//! a selected chat. It keeps Graph routes, response limits, and the outer-pin
//! to expanded-message identity check inside the integration seam. Callers
//! receive only session-scoped, inert `RemoteChatMessage` rows and cannot pin,
//! unpin, or mutate them.

use super::GraphSession;
use super::chats::{
    GraphChatMessage, normalize_message, selected_chat_identifier, valid_message_identifier,
};
use super::mutation::is_dot_segment;
use super::text::normalized_identifier;
use super::{GraphHttpError, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    AccountIdentity, MAX_REMOTE_PINNED_CHAT_MESSAGES, ProviderResourceRef,
    RemotePinnedChatMessagesProjection,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde::Deserializer;
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

const MAX_PINNED_MESSAGES_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_PINNED_MESSAGE_ITEMS: usize = MAX_REMOTE_PINNED_CHAT_MESSAGES * 2;
const MAX_PINNED_MESSAGE_FIELDS: usize = 8;

/// Failures are reduced to categories so provider IDs, response bodies, and
/// token-bearing URLs never leave this read adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinnedMessagesReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl PinnedMessagesReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph pinned messages are temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one bounded, explicit pinned-message view for one selected chat.
    /// It is deliberately separate from ordinary selected history so opening
    /// this context never implies a read receipt or broadens chat sync.
    pub(super) fn poll_chat_pinned_messages(
        &mut self,
        chat: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_pinned_messages_with_cancellation(chat, None)
    }

    /// Lets the controller stop an obsolete pinned-message request before its
    /// one bounded Graph response can surface after selection/account changes.
    pub(super) fn poll_chat_pinned_messages_cancellable(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_pinned_messages_with_cancellation(chat, Some(cancellation))
    }

    fn poll_chat_pinned_messages_with_cancellation(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let chat_id = selected_chat_identifier(&chat)?;
        let completed = SyncInterest::ChatPinnedMessages { chat: chat.clone() };
        check_cancelled(cancellation).map_err(PinnedMessagesReadFailure::transport_error)?;

        match fetch_pinned_messages(
            &self.session,
            chat_id,
            self.identity.as_ref(),
            cancellation,
        ) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: Some(RemotePinnedChatMessagesProjection::microsoft365(
                    chat,
                    result.messages,
                )?),
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(PinnedMessagesReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // This is a contextual selected-chat read. Preserve identity,
                // directory, and ordinary history while replacing only stale
                // pin context with its explicit access state.
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: Some(
                    RemotePinnedChatMessagesProjection::access_denied(chat)?,
                ),
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this chat's pinned messages. Verify delegated Chat.Read access and your chat membership, then reconnect."
                        .into(),
                ],
            }),
            Err(PinnedMessagesReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(PinnedMessagesReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

struct PinnedMessagesReadResult {
    messages: Vec<crate::integrations::RemoteChatMessage>,
    notices: Vec<String>,
}

/// A narrow Graph DTO. The outer `id` is the pinned chat message ID; retaining
/// an expanded row requires it to match the nested message ID exactly.
struct GraphPinnedChatMessage {
    id: Option<String>,
    message: Option<GraphChatMessage>,
}

#[derive(Deserialize)]
struct GraphPinnedMessagesCollection {
    #[serde(default, deserialize_with = "deserialize_pinned_message_items")]
    value: PinnedMessageItems,
    #[serde(
        rename = "@odata.nextLink",
        default,
        deserialize_with = "deserialize_optional_string"
    )]
    next_link: Option<String>,
}

/// The first page is deliberately shallow: retaining at most two candidate
/// rows per displayed item leaves room for malformed/duplicate Graph rows
/// without turning a contextual panel into an unbounded parser or cache.
#[derive(Default)]
struct PinnedMessageItems {
    rows: Vec<GraphPinnedChatMessage>,
    truncated: bool,
}

/// One malformed pin must not poison an otherwise safe selected-chat view.
/// Bound each object before retaining just the outer ID and nested message DTO;
/// all other Graph fields are discarded before normalization.
fn deserialize_pinned_message_items<'de, D>(deserializer: D) -> Result<PinnedMessageItems, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(Value::Array(values)) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(PinnedMessageItems::default());
    };
    let truncated = values.len() > MAX_PINNED_MESSAGE_ITEMS;
    let rows = values
        .into_iter()
        .take(MAX_PINNED_MESSAGE_ITEMS)
        .filter_map(|value| {
            let Value::Object(fields) = value else {
                return None;
            };
            if fields.len() > MAX_PINNED_MESSAGE_FIELDS {
                return None;
            }
            let id = match fields.get("id") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.clone()),
                Some(_) => return None,
            };
            let message = match fields.get("message") {
                None | Some(Value::Null) => None,
                Some(value) => serde_json::from_value(value.clone()).ok(),
            };
            Some(GraphPinnedChatMessage { id, message })
        })
        .collect();
    Ok(PinnedMessageItems { rows, truncated })
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::String(value)) => Some(value),
        None | Some(_) => None,
    })
}

fn fetch_pinned_messages(
    session: &GraphSession<'_>,
    chat_id: &str,
    current_identity: Option<&AccountIdentity>,
    cancellation: Option<&TransportCancellation>,
) -> Result<PinnedMessagesReadResult, PinnedMessagesReadFailure> {
    let url = chat_pinned_messages_url(session.cloud(), chat_id)?;
    check_cancelled(cancellation)?;
    let response = session
        .get_json(&url, MAX_PINNED_MESSAGES_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    // A cancellation can arrive while the one permitted request is in flight.
    // Never deserialize its body or surface a stale selection after that.
    check_cancelled(cancellation)?;
    validate_response_status(response.status, response.retry_after_seconds)?;
    let collection = serde_json::from_slice::<GraphPinnedMessagesCollection>(&response.body)
        .map_err(|_| PinnedMessagesReadFailure::Unavailable)?;

    let collection_was_truncated =
        collection.value.truncated || collection.value.rows.len() > MAX_REMOTE_PINNED_CHAT_MESSAGES;
    let mut seen_pinned_ids = BTreeSet::<String>::new();
    let mut messages = Vec::with_capacity(
        collection
            .value
            .rows
            .len()
            .min(MAX_REMOTE_PINNED_CHAT_MESSAGES),
    );
    for pinned in collection.value.rows {
        if messages.len() == MAX_REMOTE_PINNED_CHAT_MESSAGES {
            break;
        }
        let Some(pinned_id) = pinned.id.as_deref() else {
            continue;
        };
        // Graph IDs are opaque. Do not trim or otherwise normalize a row into
        // agreement: both identifiers have to be independently valid and
        // literally equal before the nested message becomes presentation data.
        if normalized_identifier(pinned_id).as_deref() != Some(pinned_id) {
            continue;
        }
        // A duplicate pin cannot establish which expanded row is authoritative.
        // Mark the first well-formed outer ID before inspecting its nested DTO
        // so a later duplicate cannot recover a malformed/mismatched first one.
        if !seen_pinned_ids.insert(pinned_id.to_owned()) {
            continue;
        }
        let Some(message) = pinned.message else {
            continue;
        };
        if valid_message_identifier(&message) != Some(pinned_id) {
            continue;
        }
        messages.push(normalize_message(&message, current_identity));
    }

    check_cancelled(cancellation)?;
    let mut notices = Vec::new();
    if collection_was_truncated || collection.next_link.is_some() {
        notices.push(format!(
            "Microsoft 365 pinned messages are showing at most {MAX_REMOTE_PINNED_CHAT_MESSAGES} items for this conversation."
        ));
    }
    Ok(PinnedMessagesReadResult { messages, notices })
}

fn chat_pinned_messages_url(
    cloud: GraphCloud,
    chat_id: &str,
) -> Result<Url, PinnedMessagesReadFailure> {
    // `Url` can otherwise normalize dot segments. The selected-chat identifier
    // has already passed the common strict validator, but retain the route
    // invariant locally so token-bearing requests cannot be retargeted.
    if is_dot_segment(chat_id) {
        return Err(PinnedMessagesReadFailure::Unavailable);
    }
    let mut url = cloud
        .graph_route(&["chats", chat_id, "pinnedMessages"], None)
        .map_err(|_| PinnedMessagesReadFailure::Unavailable)?;
    // This is Graph's documented expansion. We deliberately do not follow an
    // opaque continuation; the panel owns exactly one bounded response.
    url.query_pairs_mut().append_pair("$expand", "message");
    Ok(url)
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), PinnedMessagesReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(PinnedMessagesReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> PinnedMessagesReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => PinnedMessagesReadFailure::Unavailable,
    }
}

fn validate_response_status(
    status: u16,
    retry_after_seconds: Option<u64>,
) -> Result<(), PinnedMessagesReadFailure> {
    match status {
        200 => Ok(()),
        401 => Err(PinnedMessagesReadFailure::ReauthenticationRequired),
        403 => Err(PinnedMessagesReadFailure::AccessDenied),
        429 => Err(PinnedMessagesReadFailure::Throttled(retry_after_seconds)),
        _ => Err(PinnedMessagesReadFailure::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GraphHttpClient, GraphHttpError, GraphHttpResponse};
    use super::{
        MAX_PINNED_MESSAGES_RESPONSE_BYTES, MAX_REMOTE_PINNED_CHAT_MESSAGES, Microsoft365Transport,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, ProviderResourceRef, SyncInterest,
        TransportCancellation, TransportError, WorkspaceTransport,
    };
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    struct RecordedRequest {
        url: String,
        max_response_bytes: u64,
    }

    struct RecordingState {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    struct RecordingClient {
        state: Arc<RecordingState>,
    }

    impl RecordingClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> (Self, Arc<RecordingState>) {
            let state = Arc::new(RecordingState {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            });
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl GraphHttpClient for RecordingClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            _access_token: &str,
            max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.state
                .requests
                .lock()
                .expect("recorded request list must not be poisoned")
                .push(RecordedRequest {
                    url: url.as_str().into(),
                    max_response_bytes,
                });
            self.state
                .responses
                .lock()
                .expect("configured responses must not be poisoned")
                .pop_front()
                .expect("one response must be configured per request")
        }
    }

    struct CancellingClient {
        cancellation: TransportCancellation,
        calls: Mutex<usize>,
    }

    impl GraphHttpClient for CancellingClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            _url: &Url,
            _access_token: &str,
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            *self
                .calls
                .lock()
                .expect("cancellation call count must not be poisoned") += 1;
            self.cancellation.cancel();
            Ok(response(200, json!({"value": []})))
        }
    }

    fn response(status: u16, body: Value) -> GraphHttpResponse {
        GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.to_string().into_bytes(),
        }
    }

    fn response_with_retry_after(status: u16, retry_after_seconds: u64) -> GraphHttpResponse {
        GraphHttpResponse {
            status,
            retry_after_seconds: Some(retry_after_seconds),
            body: Vec::new(),
        }
    }

    fn connected_transport(client: Box<dyn GraphHttpClient>) -> Microsoft365Transport {
        let mut transport = Microsoft365Transport::with_client("sensitive-access-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada Lovelace".into(),
            email: Some("ada@example.test".into()),
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        transport
    }

    fn pinned_interest(chat: ProviderResourceRef) -> SyncInterest {
        SyncInterest::ChatPinnedMessages { chat }
    }

    fn expanded_message(id: &str, content: &str) -> Value {
        json!({
            "id": id,
            "body": {"contentType": "html", "content": content},
            "from": {"user": {"id": "u1", "displayName": "Ada Lovelace"}},
            "createdDateTime": "2026-11-06T12:00:00Z"
        })
    }

    #[test]
    fn reads_the_documented_encoded_pinned_route_and_normalizes_html() {
        let (client, state) = RecordingClient::new([Ok(response(
            200,
            json!({
                "value": [{
                    "id": "message-1",
                    "message": expanded_message("message-1", "<p>Pinned <b>message</b></p>")
                }]
            }),
        ))]);
        let mut transport = connected_transport(Box::new(client));
        let chat = ProviderResourceRef::microsoft365("19:chat id@thread.v2");

        let delta = transport
            .poll(pinned_interest(chat.clone()))
            .expect("pinned-message response must normalize");

        assert_eq!(delta.completed, [pinned_interest(chat.clone())]);
        assert!(delta.remote_chat_messages.is_none());
        let projection = delta
            .remote_pinned_chat_messages
            .expect("selected pinned-message projection must be present");
        assert_eq!(projection.chat(), chat);
        assert!(!projection.is_access_denied());
        assert_eq!(projection.messages().len(), 1);
        assert_eq!(projection.messages()[0].plain_text(), "Pinned message");
        assert!(projection.messages()[0].sent_by_current_user());

        let requests = state
            .requests
            .lock()
            .expect("recorded request list must not be poisoned");
        assert_eq!(requests.len(), 1, "the adapter must make one bounded read");
        assert_eq!(
            requests[0].max_response_bytes,
            MAX_PINNED_MESSAGES_RESPONSE_BYTES
        );
        let route = Url::parse(&requests[0].url).expect("recorded route must be a URL");
        assert_eq!(route.scheme(), "https");
        assert_eq!(route.host_str(), Some("graph.microsoft.com"));
        assert_eq!(
            route
                .path_segments()
                .expect("Graph route must have path segments")
                .collect::<Vec<_>>(),
            ["v1.0", "chats", "19:chat%20id@thread.v2", "pinnedMessages"]
        );
        assert!(!requests[0].url.contains("chat id"));
        assert_eq!(
            route
                .query_pairs()
                .find(|(key, _)| key == "$expand")
                .map(|(_, value)| value.into_owned()),
            Some("message".into())
        );
    }

    #[test]
    fn skips_malformed_mismatched_and_duplicate_pinned_rows() {
        let (client, _state) = RecordingClient::new([Ok(response(
            200,
            json!({
                "value": [
                    {"id": "mismatch", "message": expanded_message("different", "Wrong target")},
                    {"id": "whitespace ", "message": expanded_message("whitespace", "Raw IDs must match exactly")},
                    {"id": "duplicate", "message": expanded_message("duplicate", "First valid pin")},
                    {"id": "duplicate", "message": expanded_message("duplicate", "Duplicate pin")},
                    {"id": 9, "message": expanded_message("bad-id", "Malformed outer ID")},
                    {"id": "bad-message", "message": "not an object"},
                    {"id": "valid", "message": expanded_message("valid", "Valid pin")}
                ]
            }),
        ))]);
        let mut transport = connected_transport(Box::new(client));

        let projection = transport
            .poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:pinned@thread.v2",
            )))
            .expect("malformed optional rows must not poison the collection")
            .remote_pinned_chat_messages
            .expect("a safe projection must still be returned");

        assert_eq!(projection.messages().len(), 2);
        assert_eq!(projection.messages()[0].plain_text(), "First valid pin");
        assert_eq!(projection.messages()[1].plain_text(), "Valid pin");
    }

    #[test]
    fn bounds_the_first_page_and_never_follows_a_continuation() {
        let values = (0..=MAX_REMOTE_PINNED_CHAT_MESSAGES)
            .map(|index| {
                let id = format!("pin-{index}");
                json!({"id": id, "message": expanded_message(&format!("pin-{index}"), "Pinned")})
            })
            .collect::<Vec<_>>();
        let (client, state) = RecordingClient::new([Ok(response(
            200,
            json!({
                "value": values,
                "@odata.nextLink": "https://untrusted.example/opaque-page"
            }),
        ))]);
        let mut transport = connected_transport(Box::new(client));

        let delta = transport
            .poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:bounded@thread.v2",
            )))
            .expect("the first bounded page must remain useful");
        let projection = delta
            .remote_pinned_chat_messages
            .expect("bounded projection must be present");
        assert_eq!(projection.messages().len(), MAX_REMOTE_PINNED_CHAT_MESSAGES);
        assert_eq!(delta.notices.len(), 1);
        assert!(delta.notices[0].contains("at most"));
        assert!(!delta.notices[0].contains("untrusted"));
        assert_eq!(
            state
                .requests
                .lock()
                .expect("recorded request list must not be poisoned")
                .len(),
            1,
            "opaque continuations must never become token-bearing requests"
        );
    }

    #[test]
    fn stops_normalizing_after_the_bounded_pinned_item_count() {
        let mut values = (0..(MAX_REMOTE_PINNED_CHAT_MESSAGES * 2))
            .map(|_| json!({"id": 9, "message": expanded_message("ignored", "Malformed")}))
            .collect::<Vec<_>>();
        values.push(json!({
            "id": "outside-item-bound",
            "message": expanded_message("outside-item-bound", "Must not be reached")
        }));
        let (client, _state) = RecordingClient::new([Ok(response(200, json!({"value": values})))]);
        let mut transport = connected_transport(Box::new(client));

        let delta = transport
            .poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:item-bound@thread.v2",
            )))
            .expect("the bounded response must remain a safe empty projection");
        let projection = delta
            .remote_pinned_chat_messages
            .expect("a selected pinned-message projection must be returned");

        assert!(projection.messages().is_empty());
        assert_eq!(delta.notices.len(), 1);
    }

    #[test]
    fn scoped_denial_keeps_identity_but_reauthentication_clears_it() {
        let (denied_client, _denied_state) = RecordingClient::new([Ok(response(403, json!({})))]);
        let mut denied_transport = connected_transport(Box::new(denied_client));
        let denied = denied_transport
            .poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:denied@thread.v2",
            )))
            .expect("a scoped denial must become a scoped replacement");
        assert!(
            denied
                .remote_pinned_chat_messages
                .expect("denial projection must be present")
                .is_access_denied()
        );
        assert!(denied_transport.identity.is_some());
        assert!(matches!(
            denied_transport.status,
            ConnectionStatus::Connected(_)
        ));

        let (reauth_client, _reauth_state) = RecordingClient::new([Ok(response(401, json!({})))]);
        let mut reauth_transport = connected_transport(Box::new(reauth_client));
        assert_eq!(
            reauth_transport.poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:reauth@thread.v2",
            ))),
            Err(TransportError::ReauthenticationRequired)
        );
        assert!(reauth_transport.identity.is_none());
        assert!(matches!(
            reauth_transport.status,
            ConnectionStatus::ReauthenticationRequired
        ));
    }

    #[test]
    fn throttling_preserves_the_provider_retry_hint_and_local_targets_never_network() {
        let (throttled_client, _throttled_state) =
            RecordingClient::new([Ok(response_with_retry_after(429, 17))]);
        let mut throttled_transport = connected_transport(Box::new(throttled_client));
        assert_eq!(
            throttled_transport.poll(pinned_interest(ProviderResourceRef::microsoft365(
                "19:throttled@thread.v2",
            ))),
            Err(TransportError::Throttled {
                retry_after_seconds: Some(17),
            })
        );

        let (local_client, local_state) = RecordingClient::new([]);
        let mut local_transport = connected_transport(Box::new(local_client));
        assert!(matches!(
            local_transport.poll(pinned_interest(ProviderResourceRef::local_preview(
                "fixture-chat"
            ))),
            Err(TransportError::Unsupported(_))
        ));
        assert!(
            local_state
                .requests
                .lock()
                .expect("recorded request list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn server_and_network_failures_are_unavailable_without_provider_details() {
        for outcome in [
            Ok(response(503, json!({"error": "private Graph response"}))),
            Err(GraphHttpError::Network),
        ] {
            let (client, _state) = RecordingClient::new([outcome]);
            let mut transport = connected_transport(Box::new(client));

            let error = transport
                .poll(pinned_interest(ProviderResourceRef::microsoft365(
                    "19:private-pinned@thread.v2",
                )))
                .expect_err("server and network failures must not produce a pin projection");

            assert!(matches!(error, TransportError::Unavailable(_)));
            assert!(!error.to_string().contains("private"));
        }
    }

    #[test]
    fn cancellation_before_io_makes_no_pinned_message_request() {
        let cancellation = TransportCancellation::new();
        cancellation.cancel();
        let (client, state) = RecordingClient::new([]);
        let mut transport = connected_transport(Box::new(client));

        assert_eq!(
            transport.poll_cancellable(
                pinned_interest(ProviderResourceRef::microsoft365("19:cancel@thread.v2")),
                &cancellation,
            ),
            Err(TransportError::Cancelled)
        );
        assert!(
            state
                .requests
                .lock()
                .expect("recorded request list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn cancellation_after_the_one_request_discards_its_projection() {
        let cancellation = TransportCancellation::new();
        let client = CancellingClient {
            cancellation: cancellation.clone(),
            calls: Mutex::new(0),
        };
        let mut transport = connected_transport(Box::new(client));

        assert_eq!(
            transport.poll_cancellable(
                pinned_interest(ProviderResourceRef::microsoft365("19:cancel@thread.v2")),
                &cancellation,
            ),
            Err(TransportError::Cancelled)
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url = super::chat_pinned_messages_url(cloud, "c")
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
