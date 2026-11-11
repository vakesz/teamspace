//! Microsoft Graph selected-chat member-roster normalization.
//!
//! This module owns the one bounded `chats/{id}/members` read for a selected
//! chat. Graph routes, response limits, and conversationMember DTOs stay inside
//! the integration seam. Callers receive only inert display names and a safe
//! role label; member IDs, emails, and tenant data are discarded here.

use super::GraphSession;
use super::chats::selected_chat_identifier;
use super::mutation::is_dot_segment;
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    MAX_REMOTE_CHAT_MEMBERS, ProviderResourceRef, RemoteChatMember, RemoteChatMemberRole,
    RemoteChatMembersProjection,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde_json::Value;
use url::Url;

const MAX_MEMBERS_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_MEMBER_DISPLAY_NAME_BYTES: usize = 128;
const MAX_MEMBER_ROLES: usize = 8;

/// Failures are reduced to categories so provider IDs, response bodies, and
/// token-bearing URLs never leave this read adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MembersReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl MembersReadFailure {
    pub(super) fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph chat members are temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one bounded, explicit member roster for one selected chat.
    pub(super) fn poll_chat_members(
        &mut self,
        chat: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_members_with_cancellation(chat, None)
    }

    /// Lets the controller stop an obsolete roster request before its one
    /// bounded Graph response can surface after selection/account changes.
    pub(super) fn poll_chat_members_cancellable(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_members_with_cancellation(chat, Some(cancellation))
    }

    fn poll_chat_members_with_cancellation(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let chat_id = selected_chat_identifier(&chat)?;
        let completed = SyncInterest::ChatMembers { chat: chat.clone() };
        check_cancelled(cancellation).map_err(MembersReadFailure::transport_error)?;

        match fetch_chat_members(&self.session, chat_id, cancellation) {
            Ok(mut result) => {
                // One presence query for this explicit open. The IDs stay in
                // the adapter and never reach the projection.
                let ids = std::mem::take(&mut result.ids);
                // The same roster read also issues the mention handles. The
                // IDs go into the adapter's one ID map, never the projection.
                let roster = ids
                    .iter()
                    .cloned()
                    .zip(result.members.iter().map(|member| member.display_name().to_owned()))
                    .collect();
                let handles = self.member_ids.remember_roster(chat_id, roster);
                for (member, handle) in result.members.iter_mut().zip(handles) {
                    member.set_mention(handle);
                }
                let presence = self.initial_member_presence(ids, cancellation)?;
                for (member, presence) in result.members.iter_mut().zip(presence.rows) {
                    member.set_presence(presence);
                }
                let mut notices = result.notices;
                notices.extend(presence.notice);
                Ok(WorkspaceDelta {
                    completed: vec![completed],
                    remote_chat_members: Some(
                        RemoteChatMembersProjection::microsoft365(chat, result.members)?
                            .with_presence_status(presence.status),
                    ),
                    remote_capabilities: None,
                    notices,
                    ..WorkspaceDelta::default()
                })
            }
            Err(MembersReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // A contextual read: preserve identity, directory, history and
                // pins while replacing only the stale roster with its denial.
                completed: vec![completed],
                remote_chat_members: Some(RemoteChatMembersProjection::access_denied(chat)?),
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this chat's members. Verify delegated Chat.Read access and your chat membership, then reconnect."
                        .into(),
                ],
                ..WorkspaceDelta::default()
            }),
            Err(MembersReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(MembersReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

pub(super) struct MembersReadResult {
    members: Vec<RemoteChatMember>,
    /// Positional with `members`; adapter-private and taken before the seam.
    pub(super) ids: Vec<Option<String>>,
    notices: Vec<String>,
}

pub(super) fn fetch_chat_members(
    session: &GraphSession<'_>,
    chat_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<MembersReadResult, MembersReadFailure> {
    let url = chat_members_url(session.cloud(), chat_id)?;
    check_cancelled(cancellation)?;
    let response = session
        .get_json(&url, MAX_MEMBERS_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    // Never deserialize a body or surface a stale selection after cancellation.
    check_cancelled(cancellation)?;
    validate_response_status(response.status, response.retry_after_seconds)?;
    let mut body = serde_json::from_slice::<Value>(&response.body)
        .map_err(|_| MembersReadFailure::Unavailable)?;

    let next_link_present = body
        .get("@odata.nextLink")
        .is_some_and(|link| !link.is_null());
    let rows = match body.get_mut("value").map(Value::take) {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    };
    let truncated = next_link_present || rows.len() > MAX_REMOTE_CHAT_MEMBERS;
    let (members, ids): (Vec<_>, Vec<_>) = rows
        .iter()
        .take(MAX_REMOTE_CHAT_MEMBERS)
        .filter_map(normalize_member)
        .unzip();

    check_cancelled(cancellation)?;
    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Microsoft 365 chat members are showing at most {MAX_REMOTE_CHAT_MEMBERS} people for this conversation."
        ));
    }
    Ok(MembersReadResult {
        members,
        ids,
        notices,
    })
}

/// Lenient per-entry normalization: a malformed entry is skipped and never
/// poisons the roster. Only a sanitized (possibly empty) name and a known role
/// survive; the feature supplies its own placeholder for an empty name.
fn normalize_member(value: &Value) -> Option<(RemoteChatMember, Option<String>)> {
    let fields = value.as_object()?;
    let display_name = fields
        .get("displayName")
        .and_then(Value::as_str)
        .map(|name| safe_display(name, MAX_MEMBER_DISPLAY_NAME_BYTES))
        .unwrap_or_default();
    let roles = fields
        .get("roles")
        .and_then(Value::as_array)
        .map(|roles| {
            roles
                .iter()
                .take(MAX_MEMBER_ROLES)
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let role = if roles.iter().any(|role| role == "owner") {
        Some(RemoteChatMemberRole::Owner)
    } else if roles.iter().any(|role| role == "guest") {
        Some(RemoteChatMemberRole::Guest)
    } else {
        None
    };
    // The user ID is read only from an Entra user member; it never enters the
    // row and is used solely by the adapter's presence query.
    let user_id = (fields.get("@odata.type").and_then(Value::as_str)
        == Some("#microsoft.graph.aadUserConversationMember"))
    .then(|| fields.get("userId").and_then(Value::as_str))
    .flatten()
    .and_then(normalized_identifier);
    Some((RemoteChatMember::microsoft365(display_name, role), user_id))
}

fn chat_members_url(cloud: GraphCloud, chat_id: &str) -> Result<Url, MembersReadFailure> {
    // `Url` can otherwise normalize dot segments; retain the route invariant
    // locally so token-bearing requests cannot be retargeted.
    if is_dot_segment(chat_id) {
        return Err(MembersReadFailure::Unavailable);
    }
    let url = cloud
        .graph_route(&["chats", chat_id, "members"], None)
        .map_err(|_| MembersReadFailure::Unavailable)?;
    Ok(url)
}

fn check_cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), MembersReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(MembersReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> MembersReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => MembersReadFailure::Unavailable,
    }
}

fn validate_response_status(
    status: u16,
    retry_after_seconds: Option<u64>,
) -> Result<(), MembersReadFailure> {
    match status {
        200 => Ok(()),
        401 => Err(MembersReadFailure::ReauthenticationRequired),
        403 => Err(MembersReadFailure::AccessDenied),
        429 => Err(MembersReadFailure::Throttled(retry_after_seconds)),
        _ => Err(MembersReadFailure::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GraphHttpClient, GraphHttpError, GraphHttpResponse};
    use super::{MAX_MEMBERS_RESPONSE_BYTES, MAX_REMOTE_CHAT_MEMBERS, Microsoft365Transport};
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

    fn members_interest(chat: ProviderResourceRef) -> SyncInterest {
        SyncInterest::ChatMembers { chat }
    }

    #[test]
    fn reads_the_documented_encoded_members_route_with_one_bounded_request() {
        let (client, state) = RecordingClient::new([Ok(response(200, json!({"value": []})))]);
        let mut transport = connected_transport(Box::new(client));
        let chat = ProviderResourceRef::microsoft365("19:chat id@thread.v2");

        let delta = transport
            .poll(members_interest(chat.clone()))
            .expect("members response must normalize");

        assert_eq!(delta.completed, [members_interest(chat.clone())]);
        let projection = delta
            .remote_chat_members
            .expect("selected members projection must be present");
        assert_eq!(projection.chat(), chat);
        assert!(!projection.is_access_denied());
        assert!(delta.remote_pinned_chat_messages.is_none());

        let requests = state
            .requests
            .lock()
            .expect("recorded request list must not be poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].max_response_bytes, MAX_MEMBERS_RESPONSE_BYTES);
        let route = Url::parse(&requests[0].url).expect("recorded route must be a URL");
        assert_eq!(route.scheme(), "https");
        assert_eq!(route.host_str(), Some("graph.microsoft.com"));
        assert_eq!(
            route
                .path_segments()
                .expect("Graph route must have path segments")
                .collect::<Vec<_>>(),
            ["v1.0", "chats", "19:chat%20id@thread.v2", "members"]
        );
        assert!(!requests[0].url.contains("chat id"));
        assert_eq!(route.query(), None);
    }

    #[test]
    fn maps_names_and_roles_and_sanitizes_display_names() {
        let (client, _state) = RecordingClient::new([Ok(response(
            200,
            json!({
                "value": [
                    {"id": "m1", "displayName": "Ada\u{0007}   Lovelace", "roles": ["owner"], "email": "ada@example.test", "userId": "u1"},
                    {"id": "m2", "displayName": "Grace Hopper", "roles": ["guest"]},
                    {"id": "m3", "displayName": "Linus", "roles": []},
                    {"id": "m4", "displayName": "Future", "roles": ["somethingNew"]},
                    {"id": "m5", "displayName": "Both", "roles": ["Guest", "OWNER"]},
                    {"id": "m6", "roles": ["owner"]}
                ]
            }),
        ))]);
        let mut transport = connected_transport(Box::new(client));

        let delta = transport
            .poll(members_interest(ProviderResourceRef::microsoft365(
                "19:chat@thread.v2",
            )))
            .expect("members response must normalize");
        let projection = delta.remote_chat_members.expect("projection present");
        let rows = projection
            .members()
            .iter()
            .map(|member| (member.display_name(), member.role_label()))
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            [
                ("Ada Lovelace", Some("Owner")),
                ("Grace Hopper", Some("Guest")),
                ("Linus", None),
                ("Future", None),
                ("Both", Some("Owner")),
                ("", Some("Owner")),
            ]
        );
        let debug = format!("{projection:?}");
        assert!(!debug.contains("ada@example.test"));
        assert!(!debug.contains("u1"));
    }

    #[test]
    fn skips_malformed_entries_and_tolerates_a_non_array_value() {
        let (client, _state) = RecordingClient::new([
            Ok(response(
                200,
                json!({"value": ["text", 7, null, {"displayName": 9, "roles": "owner"}, {"displayName": "Valid"}]}),
            )),
            Ok(response(200, json!({"value": "oops"}))),
        ]);
        let mut transport = connected_transport(Box::new(client));
        let chat = ProviderResourceRef::microsoft365("19:chat@thread.v2");

        let first = transport
            .poll(members_interest(chat.clone()))
            .expect("lenient parse")
            .remote_chat_members
            .expect("projection present");
        let names = first
            .members()
            .iter()
            .map(|member| member.display_name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["", "Valid"]);

        let second = transport
            .poll(members_interest(chat))
            .expect("non-array value is empty")
            .remote_chat_members
            .expect("projection present");
        assert!(second.members().is_empty());
    }

    #[test]
    fn bounds_the_first_page_and_never_follows_a_continuation() {
        let rows = (0..MAX_REMOTE_CHAT_MEMBERS + 5)
            .map(|index| json!({"displayName": format!("Member {index}")}))
            .collect::<Vec<_>>();
        let (client, state) = RecordingClient::new([Ok(response(
            200,
            json!({
                "value": rows,
                "@odata.nextLink": "https://graph.microsoft.com/v1.0/chats/x/members?$skiptoken=abc"
            }),
        ))]);
        let mut transport = connected_transport(Box::new(client));

        let delta = transport
            .poll(members_interest(ProviderResourceRef::microsoft365(
                "19:chat@thread.v2",
            )))
            .expect("oversized page must truncate");

        assert_eq!(
            delta
                .remote_chat_members
                .expect("projection present")
                .members()
                .len(),
            MAX_REMOTE_CHAT_MEMBERS
        );
        assert_eq!(delta.notices.len(), 1);
        assert_eq!(
            state
                .requests
                .lock()
                .expect("recorded request list must not be poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn scoped_denial_keeps_identity_but_reauthentication_clears_it() {
        let (denied_client, _state) = RecordingClient::new([Ok(response(403, json!({})))]);
        let mut denied_transport = connected_transport(Box::new(denied_client));
        let denied = denied_transport
            .poll(members_interest(ProviderResourceRef::microsoft365(
                "19:denied@thread.v2",
            )))
            .expect("a scoped denial must become a scoped replacement");
        assert!(
            denied
                .remote_chat_members
                .expect("denial projection must be present")
                .is_access_denied()
        );
        assert!(denied_transport.identity.is_some());

        let (reauth_client, _state) = RecordingClient::new([Ok(response(401, json!({})))]);
        let mut reauth_transport = connected_transport(Box::new(reauth_client));
        assert_eq!(
            reauth_transport.poll(members_interest(ProviderResourceRef::microsoft365(
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
    fn throttling_preserves_the_retry_hint_and_local_targets_never_network() {
        let (client, _state) = RecordingClient::new([Ok(response_with_retry_after(429, 17))]);
        let mut transport = connected_transport(Box::new(client));
        assert_eq!(
            transport.poll(members_interest(ProviderResourceRef::microsoft365(
                "19:throttled@thread.v2",
            ))),
            Err(TransportError::Throttled {
                retry_after_seconds: Some(17),
            })
        );

        let (local_client, local_state) = RecordingClient::new([]);
        let mut local_transport = connected_transport(Box::new(local_client));
        assert!(matches!(
            local_transport.poll(members_interest(ProviderResourceRef::local_preview(
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
                .poll(members_interest(ProviderResourceRef::microsoft365(
                    "19:private-members@thread.v2",
                )))
                .expect_err("failures must not produce a roster");
            assert!(matches!(error, TransportError::Unavailable(_)));
            assert!(!error.to_string().contains("private"));
        }
    }

    #[test]
    fn cancellation_before_io_makes_no_members_request() {
        let cancellation = TransportCancellation::new();
        cancellation.cancel();
        let (client, state) = RecordingClient::new([]);
        let mut transport = connected_transport(Box::new(client));

        assert_eq!(
            transport.poll_cancellable(
                members_interest(ProviderResourceRef::microsoft365("19:cancel@thread.v2")),
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
        };
        let mut transport = connected_transport(Box::new(client));

        assert_eq!(
            transport.poll_cancellable(
                members_interest(ProviderResourceRef::microsoft365("19:cancel@thread.v2")),
                &cancellation,
            ),
            Err(TransportError::Cancelled)
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url =
                super::chat_members_url(cloud, "c").unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
