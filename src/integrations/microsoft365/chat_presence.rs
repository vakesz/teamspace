//! Microsoft Graph People-panel presence.
//!
//! One bounded `communications/getPresencesByUserId` request per explicit
//! People open, then at most one per refresh while the panel stays open. Member
//! user IDs exist only in this adapter: they are collected during the explicit
//! roster read, kept in a small per-transport map, and used solely to build
//! the presence query. A refresh never re-reads the Teams chat roster. Nothing
//! here crosses the seam except normalized presence categories.

use super::chats::selected_chat_identifier;
use super::mutation::is_dot_segment;
use super::{GraphHttpError, GraphJsonRequest, GraphSession, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    Capability, MAX_REMOTE_CHAT_MEMBERS, PresenceStatus, ProviderResourceRef, RemoteCapabilities,
    RemoteChatMembersProjection, RemoteMemberPresence,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_PRESENCE_RESPONSE_BYTES: u64 = 64 * 1024;
const DENIED_NOTICE: &str = "Presence needs the optional Presence.Read.All permission";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PresenceFailure {
    Cancelled,
    ReauthenticationRequired,
    Forbidden,
    Throttled(Option<u64>),
    Unavailable,
}

/// Normalizes one Graph `presence` entry. Case-variants and unknown values
/// fail closed to `None`.
fn normalize_presence(availability: &str, activity: Option<&str>) -> Option<RemoteMemberPresence> {
    let base = match availability {
        "Available" | "AvailableIdle" => RemoteMemberPresence::Available,
        "Busy" | "BusyIdle" => RemoteMemberPresence::Busy,
        "DoNotDisturb" => RemoteMemberPresence::DoNotDisturb,
        "Away" => RemoteMemberPresence::Away,
        "BeRightBack" => RemoteMemberPresence::BeRightBack,
        "Offline" => RemoteMemberPresence::Offline,
        _ => return None,
    };
    let busy_activity = matches!(
        activity,
        Some("InACall" | "InAMeeting" | "Presenting" | "Focusing")
    );
    Some(match base {
        RemoteMemberPresence::DoNotDisturb | RemoteMemberPresence::Offline => base,
        _ if busy_activity => RemoteMemberPresence::Busy,
        _ => base,
    })
}

fn presence_request_body(ids: &[Option<String>]) -> (Vec<String>, Vec<u8>) {
    let mut seen = BTreeSet::new();
    let unique: Vec<String> = ids
        .iter()
        .flatten()
        .filter(|id| seen.insert(id.as_str()))
        .take(MAX_REMOTE_CHAT_MEMBERS)
        .cloned()
        .collect();
    let body = serde_json::to_vec(&json!({ "ids": unique })).unwrap_or_default();
    (unique, body)
}

/// Sends the one presence query and joins its answer positionally to `ids`.
fn fetch_presence(
    session: &GraphSession<'_>,
    cloud: GraphCloud,
    ids: &[Option<String>],
    cancellation: Option<&TransportCancellation>,
) -> Result<Vec<Option<RemoteMemberPresence>>, PresenceFailure> {
    let (unique, body) = presence_request_body(ids);
    if unique.is_empty() {
        return Ok(vec![None; ids.len()]);
    }
    if unique.iter().any(|id| is_dot_segment(id)) {
        return Err(PresenceFailure::Unavailable);
    }
    let url = cloud
        .graph_route(&["communications", "getPresencesByUserId"], None)
        .map_err(|_| PresenceFailure::Unavailable)?;
    cancelled(cancellation)?;
    let response = session
        .post_json_query(
            &GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: &body,
            },
            MAX_PRESENCE_RESPONSE_BYTES,
        )
        .map_err(|error| match error {
            GraphHttpError::Network
            | GraphHttpError::ResponseTooLarge
            | GraphHttpError::Unsupported
            | GraphHttpError::RenewedBeforeDelivery
            | GraphHttpError::RenewalUnavailable => PresenceFailure::Unavailable,
        })?;
    // Never surface a stale selection after cancellation.
    cancelled(cancellation)?;
    match response.status {
        200 => {}
        401 => return Err(PresenceFailure::ReauthenticationRequired),
        403 => return Err(PresenceFailure::Forbidden),
        429 => return Err(PresenceFailure::Throttled(response.retry_after_seconds)),
        _ => return Err(PresenceFailure::Unavailable),
    }
    let parsed = serde_json::from_slice::<Value>(&response.body)
        .map_err(|_| PresenceFailure::Unavailable)?;
    let requested: BTreeSet<&str> = unique.iter().map(String::as_str).collect();
    let mut answers: BTreeMap<&str, Option<RemoteMemberPresence>> = BTreeMap::new();
    let mut duplicated: BTreeSet<&str> = BTreeSet::new();
    let entries = parsed
        .get("value")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for entry in entries.iter().take(MAX_REMOTE_CHAT_MEMBERS * 2) {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = requested.get(id).copied() else {
            continue; // unrequested ids are ignored
        };
        let presence = entry
            .get("availability")
            .and_then(Value::as_str)
            .and_then(|availability| {
                normalize_presence(availability, entry.get("activity").and_then(Value::as_str))
            });
        if answers.insert(id, presence).is_some() {
            duplicated.insert(id);
        }
    }
    Ok(ids
        .iter()
        .map(|id| {
            let id = id.as_deref()?;
            if duplicated.contains(id) {
                return None;
            }
            answers.get(id).copied().flatten()
        })
        .collect())
}

fn cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), PresenceFailure> {
    if cancellation.is_some_and(TransportCancellation::is_cancelled) {
        return Err(PresenceFailure::Cancelled);
    }
    Ok(())
}

/// What the roster read learned about presence, for the one projection.
pub(super) struct InitialPresence {
    pub(super) rows: Vec<Option<RemoteMemberPresence>>,
    pub(super) status: PresenceStatus,
    pub(super) notice: Option<String>,
}

impl Microsoft365Transport {
    fn presence_capability(&self) -> Capability {
        RemoteCapabilities::compute(
            self.session.cloud(),
            self.session.granted_scope().as_deref(),
        )
        .member_presence
    }

    /// Runs after an explicit People open. A presence failure is never fatal:
    /// the rows stay and only the presence status says why labels are absent.
    pub(super) fn initial_member_presence(
        &mut self,
        ids: Vec<Option<String>>,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<InitialPresence, TransportError> {
        let row_count = ids.len();
        let none = |status, notice: Option<&str>| InitialPresence {
            rows: vec![None; row_count],
            status,
            notice: notice.map(str::to_owned),
        };
        match self.presence_capability() {
            Capability::UnsupportedInCloud => {
                return Ok(none(PresenceStatus::UnsupportedInCloud, None));
            }
            Capability::NotGranted => return Ok(none(PresenceStatus::NotGranted, None)),
            Capability::Available | Capability::Unknown => {}
        }
        if self.presence_denied {
            return Ok(none(PresenceStatus::NotGranted, None));
        }
        let cloud = self.session.cloud();
        match fetch_presence(&self.session, cloud, &ids, cancellation) {
            Ok(rows) => Ok(InitialPresence {
                rows,
                status: PresenceStatus::Available,
                notice: None,
            }),
            Err(PresenceFailure::Forbidden) => {
                self.presence_denied = true;
                Ok(none(PresenceStatus::NotGranted, Some(DENIED_NOTICE)))
            }
            Err(PresenceFailure::Throttled(_)) => Ok(none(PresenceStatus::Throttled, None)),
            Err(PresenceFailure::Unavailable) => Ok(none(PresenceStatus::Unavailable, None)),
            Err(PresenceFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(PresenceFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
        }
    }

    /// One presence-only refresh. It issues no Teams read: with no retained
    /// IDs for the chat it does nothing until People is reopened.
    pub(super) fn poll_chat_member_presence(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let chat_id = selected_chat_identifier(&chat)?.to_owned();
        if cancellation.is_some_and(TransportCancellation::is_cancelled) {
            return Err(TransportError::Cancelled);
        }
        let completed = vec![SyncInterest::ChatMemberPresence { chat: chat.clone() }];
        let update = |updates, status| -> Result<RemoteChatMembersProjection, TransportError> {
            RemoteChatMembersProjection::presence_update(chat.clone(), updates, status)
        };
        let blocked = match self.presence_capability() {
            Capability::UnsupportedInCloud => Some(PresenceStatus::UnsupportedInCloud),
            Capability::NotGranted => Some(PresenceStatus::NotGranted),
            Capability::Available | Capability::Unknown if self.presence_denied => {
                Some(PresenceStatus::NotGranted)
            }
            _ => None,
        };
        if let Some(status) = blocked {
            return Ok(WorkspaceDelta {
                completed,
                remote_chat_members: Some(update(Vec::new(), status)?),
                ..WorkspaceDelta::default()
            });
        }
        let Some(ids) = self.member_ids.ids_for(&chat_id) else {
            return Ok(WorkspaceDelta {
                completed,
                ..WorkspaceDelta::default()
            });
        };
        let cloud = self.session.cloud();
        match fetch_presence(&self.session, cloud, &ids, cancellation) {
            Ok(rows) => Ok(WorkspaceDelta {
                completed,
                remote_chat_members: Some(update(rows, PresenceStatus::Available)?),
                ..WorkspaceDelta::default()
            }),
            Err(PresenceFailure::Forbidden) => {
                self.presence_denied = true;
                Ok(WorkspaceDelta {
                    completed,
                    remote_chat_members: Some(update(Vec::new(), PresenceStatus::NotGranted)?),
                    notices: vec![DENIED_NOTICE.into()],
                    ..WorkspaceDelta::default()
                })
            }
            Err(PresenceFailure::Throttled(retry_after_seconds)) => {
                Err(TransportError::Throttled {
                    retry_after_seconds,
                })
            }
            Err(PresenceFailure::Unavailable) => Err(TransportError::Unavailable(
                "Microsoft Graph presence is temporarily unavailable".into(),
            )),
            Err(PresenceFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(PresenceFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GraphHttpClient, GraphHttpError, GraphHttpResponse, GraphJsonRequest};
    use super::*;
    use crate::integrations::{AccountIdentity, ConnectionStatus, WorkspaceTransport};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    #[derive(Default)]
    struct State {
        gets: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        posts: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        get_urls: Mutex<Vec<String>>,
        post_urls: Mutex<Vec<String>>,
        post_bodies: Mutex<Vec<String>>,
        post_limits: Mutex<Vec<u64>>,
        cancel_on_get: Mutex<Option<TransportCancellation>>,
    }

    struct Client(Arc<State>);

    impl GraphHttpClient for Client {
        fn get_me(&self, _: &Url, _: &str) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn get_json(
            &self,
            url: &Url,
            _: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0.get_urls.lock().unwrap().push(url.as_str().into());
            if let Some(cancel) = self.0.cancel_on_get.lock().unwrap().as_ref() {
                cancel.cancel();
            }
            self.0
                .gets
                .lock()
                .unwrap()
                .pop_front()
                .expect("get scripted")
        }

        fn post_json_query(
            &self,
            request: GraphJsonRequest<'_>,
            _: &str,
            limit: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .post_urls
                .lock()
                .unwrap()
                .push(request.url.as_str().into());
            self.0
                .post_bodies
                .lock()
                .unwrap()
                .push(String::from_utf8(request.body.to_vec()).unwrap());
            self.0.post_limits.lock().unwrap().push(limit);
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

    fn aad(name: &str, user: &str) -> Value {
        json!({"@odata.type": "#microsoft.graph.aadUserConversationMember",
               "displayName": name, "userId": user, "roles": []})
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

    fn chat() -> ProviderResourceRef {
        ProviderResourceRef::microsoft365("19:chat@thread.v2")
    }

    fn open(transport: &mut Microsoft365Transport) -> WorkspaceDelta {
        transport
            .poll(SyncInterest::ChatMembers { chat: chat() })
            .expect("members open")
    }

    fn refresh(transport: &mut Microsoft365Transport) -> Result<WorkspaceDelta, TransportError> {
        transport.poll(SyncInterest::ChatMemberPresence { chat: chat() })
    }

    fn posts(state: &State) -> usize {
        state.post_bodies.lock().unwrap().len()
    }

    fn two_members() -> Value {
        json!({"value": [aad("Ada", "id-ada"), aad("Bob", "id-bob")]})
    }

    #[test]
    fn presence_body_is_exact_and_deduplicated_and_joined_by_id() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(
            200,
            json!({"value": [aad("Ada", "id-ada"), aad("Ada2", "id-ada"), aad("Bob", "id-bob")]}),
        ));
        state.posts.lock().unwrap().push_back(resp(
            200,
            json!({"value": [
                {"id": "id-ada", "availability": "AvailableIdle", "activity": "Available"},
                {"id": "id-bob", "availability": "Away", "activity": "InAMeeting"}
            ]}),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let delta = open(&mut t);
        assert_eq!(
            state.post_bodies.lock().unwrap()[0],
            r#"{"ids":["id-ada","id-bob"]}"#
        );
        assert_eq!(
            state.post_limits.lock().unwrap()[0],
            MAX_PRESENCE_RESPONSE_BYTES
        );
        assert!(
            state.post_urls.lock().unwrap()[0].ends_with("/communications/getPresencesByUserId")
        );
        let projection = delta.remote_chat_members.unwrap();
        let got: Vec<_> = projection.members().iter().map(|m| m.presence()).collect();
        assert_eq!(
            got,
            [
                Some(RemoteMemberPresence::Available),
                Some(RemoteMemberPresence::Available),
                Some(RemoteMemberPresence::Busy)
            ]
        );
        assert_eq!(projection.presence_status(), PresenceStatus::Available);
        let debug = format!("{projection:?} {:?}", t.member_ids);
        assert!(!debug.contains("id-ada") && !debug.contains("id-bob"));
    }

    #[test]
    fn non_aad_missing_or_malformed_ids_get_no_presence_and_no_post() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(
            200,
            json!({"value": [
                {"@odata.type": "#microsoft.graph.anonymousGuestConversationMember", "displayName": "G", "userId": "id-g"},
                {"@odata.type": "#microsoft.graph.aadUserConversationMember", "displayName": "NoId"},
                {"@odata.type": "#microsoft.graph.aadUserConversationMember", "displayName": "Dot", "userId": ".."},
                {"displayName": "Untyped", "userId": "id-u"}
            ]}),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let delta = open(&mut t);
        assert_eq!(posts(&state), 0);
        let projection = delta.remote_chat_members.unwrap();
        assert!(projection.members().iter().all(|m| m.presence().is_none()));
        assert_eq!(t.member_ids.len_for_test(), 0);
    }

    #[test]
    fn duplicate_unrequested_unknown_and_case_variant_entries_fail_closed() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(
            200,
            json!({"value": [aad("A", "a"), aad("B", "b"), aad("C", "c"), aad("D", "d")]}),
        ));
        state.posts.lock().unwrap().push_back(resp(
            200,
            json!({"value": [
                {"id": "a", "availability": "Busy"},
                {"id": "a", "availability": "Available"},
                {"id": "zzz", "availability": "Busy"},
                {"id": "b", "availability": "PresenceUnknown"},
                {"id": "c", "availability": "busy"},
                {"id": "d", "availability": "DoNotDisturb", "activity": "Presenting"}
            ]}),
        ));
        let mut t = transport(&state, GraphCloud::Global);
        let projection = open(&mut t).remote_chat_members.unwrap();
        let got: Vec<_> = projection.members().iter().map(|m| m.presence()).collect();
        assert_eq!(
            got,
            [None, None, None, Some(RemoteMemberPresence::DoNotDisturb)]
        );
    }

    #[test]
    fn refresh_posts_only_the_presence_query_and_never_rereads_the_roster() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        for availability in ["Available", "Offline"] {
            state.posts.lock().unwrap().push_back(resp(
                200,
                json!({"value": [
                    {"id": "id-ada", "availability": availability},
                    {"id": "id-bob", "availability": "BeRightBack"}
                ]}),
            ));
        }
        let mut t = transport(&state, GraphCloud::Global);
        open(&mut t);
        let delta = refresh(&mut t).expect("refresh");
        assert_eq!(state.get_urls.lock().unwrap().len(), 1, "roster read once");
        assert_eq!(posts(&state), 2);
        let projection = delta.remote_chat_members.unwrap();
        assert!(projection.is_presence_update());
        assert_eq!(
            projection.presence_updates(),
            [
                Some(RemoteMemberPresence::Offline),
                Some(RemoteMemberPresence::BeRightBack)
            ]
        );
        assert!(projection.members().is_empty());
        assert!(!format!("{projection:?}").contains("id-ada"));
    }

    #[test]
    fn refresh_without_remembered_ids_is_a_no_op() {
        let state = Arc::new(State::default());
        let mut t = transport(&state, GraphCloud::Global);
        let delta = refresh(&mut t).expect("no-op");
        assert!(delta.remote_chat_members.is_none());
        assert_eq!(posts(&state), 0);
        assert!(state.get_urls.lock().unwrap().is_empty());
    }

    #[test]
    fn forbidden_sets_not_granted_with_one_notice_and_no_later_post() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        state.posts.lock().unwrap().push_back(resp(403, json!({})));
        let mut t = transport(&state, GraphCloud::Global);
        let delta = open(&mut t);
        assert_eq!(delta.notices, [DENIED_NOTICE]);
        assert_eq!(
            delta.remote_chat_members.unwrap().presence_status(),
            PresenceStatus::NotGranted
        );
        let again = refresh(&mut t).expect("blocked refresh");
        assert_eq!(
            again.remote_chat_members.unwrap().presence_status(),
            PresenceStatus::NotGranted
        );
        assert!(again.notices.is_empty());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        open(&mut t);
        assert_eq!(posts(&state), 1);
    }

    #[test]
    fn china_never_posts() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        let mut t = transport(&state, GraphCloud::China);
        let projection = open(&mut t).remote_chat_members.unwrap();
        assert_eq!(
            projection.presence_status(),
            PresenceStatus::UnsupportedInCloud
        );
        let refreshed = refresh(&mut t).unwrap().remote_chat_members.unwrap();
        assert_eq!(
            refreshed.presence_status(),
            PresenceStatus::UnsupportedInCloud
        );
        assert_eq!(posts(&state), 0);
    }

    #[test]
    fn initial_failures_are_non_fatal_and_keep_rows() {
        for (reply, status) in [
            (resp(429, json!({})), PresenceStatus::Throttled),
            (resp(500, json!({})), PresenceStatus::Unavailable),
            (
                Err(GraphHttpError::ResponseTooLarge),
                PresenceStatus::Unavailable,
            ),
        ] {
            let state = Arc::new(State::default());
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, two_members()));
            state.posts.lock().unwrap().push_back(reply);
            let mut t = transport(&state, GraphCloud::Global);
            let projection = open(&mut t).remote_chat_members.unwrap();
            assert_eq!(projection.members().len(), 2);
            assert_eq!(projection.presence_status(), status);
        }
    }

    #[test]
    fn refresh_throttle_and_server_errors_are_typed_errors() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": []})));
        state.posts.lock().unwrap().push_back(Ok(GraphHttpResponse {
            status: 429,
            retry_after_seconds: Some(42),
            body: Vec::new(),
        }));
        state.posts.lock().unwrap().push_back(resp(503, json!({})));
        let mut t = transport(&state, GraphCloud::Global);
        open(&mut t);
        assert_eq!(
            refresh(&mut t).unwrap_err(),
            TransportError::Throttled {
                retry_after_seconds: Some(42)
            }
        );
        assert!(matches!(
            refresh(&mut t).unwrap_err(),
            TransportError::Unavailable(_)
        ));
    }

    #[test]
    fn one_hundred_ids_are_the_bound_and_extra_rows_are_never_queried() {
        let state = Arc::new(State::default());
        let rows: Vec<Value> = (0..120).map(|i| aad("M", &format!("id-{i}"))).collect();
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, json!({ "value": rows })));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": []})));
        let mut t = transport(&state, GraphCloud::Global);
        open(&mut t);
        let body: Value = serde_json::from_str(&state.post_bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["ids"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn cancellation_between_get_and_post_stops_the_post() {
        let state = Arc::new(State::default());
        let cancellation = TransportCancellation::default();
        *state.cancel_on_get.lock().unwrap() = Some(cancellation.clone());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        let mut t = transport(&state, GraphCloud::Global);
        let result = t.poll_cancellable(SyncInterest::ChatMembers { chat: chat() }, &cancellation);
        assert_eq!(result.unwrap_err(), TransportError::Cancelled);
        assert_eq!(posts(&state), 0);
    }

    #[test]
    fn reauthentication_clears_remembered_ids() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": []})));
        let mut t = transport(&state, GraphCloud::Global);
        open(&mut t);
        assert_eq!(t.member_ids.len_for_test(), 1);
        t.record_error(&TransportError::ReauthenticationRequired);
        assert_eq!(t.member_ids.len_for_test(), 0);
    }

    fn members_projection(
        delta: &WorkspaceDelta,
    ) -> Vec<Option<crate::integrations::transport::MentionHandle>> {
        delta
            .remote_chat_members
            .as_ref()
            .expect("projection")
            .members()
            .iter()
            .map(|member| member.mention())
            .collect()
    }

    #[test]
    fn roster_fills_directory_even_without_presence_grant() {
        // The China cloud never offers presence, yet mentions still need IDs.
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        let mut t = transport(&state, GraphCloud::China);
        let delta = open(&mut t);
        assert_eq!(posts(&state), 0);
        let handles = members_projection(&delta);
        assert!(handles.iter().all(Option::is_some));
        assert_eq!(t.member_ids.len_for_test(), 1);
        let ids = t
            .member_ids
            .ids_for("19:chat@thread.v2")
            .expect("remembered");
        assert_eq!(ids, vec![Some("id-ada".into()), Some("id-bob".into())]);
    }

    #[test]
    fn a_presence_denial_no_longer_forgets_mention_handles() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        state.posts.lock().unwrap().push_back(resp(403, json!({})));
        let mut t = transport(&state, GraphCloud::Global);
        let delta = open(&mut t);
        let handles = members_projection(&delta);
        assert!(handles.iter().all(Option::is_some));
        assert_eq!(t.member_ids.len_for_test(), 1);
        // The handle still resolves to its member after the denial.
        let ada = handles[0].expect("handle");
        assert_eq!(
            t.member_ids.resolve("19:chat@thread.v2", ada),
            Some(("id-ada", "Ada"))
        );
    }

    #[test]
    fn reopening_people_keeps_each_members_handle() {
        let state = Arc::new(State::default());
        for _ in 0..2 {
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, two_members()));
        }
        let mut t = transport(&state, GraphCloud::China);
        let first = members_projection(&open(&mut t));
        let second = members_projection(&open(&mut t));
        assert_eq!(first, second);
    }

    #[test]
    fn member_projection_debug_shows_no_id_or_handle_value() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        let mut t = transport(&state, GraphCloud::China);
        let delta = open(&mut t);
        let debug = format!("{:?} {:?}", delta.remote_chat_members, t.member_ids);
        assert!(!debug.contains("id-ada") && !debug.contains("id-bob"));
        assert!(debug.contains("MentionHandle(..)"));
    }

    fn mention(user: &str) -> Value {
        json!({"id": 0, "mentionText": "x", "mentioned": {"user": {"id": user, "displayName": "x"}}})
    }

    fn message(id: &str, mentions: Vec<Value>, deleted: bool) -> Value {
        let mut value = json!({
            "id": id,
            "messageType": "message",
            "body": {"contentType": "text", "content": "hi"},
            "createdDateTime": format!("2026-11-06T11:00:0{}Z", id.trim_start_matches('m')),
            "mentions": mentions,
        });
        if deleted {
            value["deletedDateTime"] = json!("2026-11-06T12:00:00Z");
        }
        value
    }

    fn read_history(
        t: &mut Microsoft365Transport,
        chat: &ProviderResourceRef,
    ) -> Vec<Vec<crate::integrations::transport::MentionHandle>> {
        let delta = t
            .poll(SyncInterest::ChatMessages { chat: chat.clone() })
            .expect("history reads");
        delta
            .remote_chat_messages
            .expect("history projection")
            .messages()
            .iter()
            .map(|message| message.mentioned_handles().to_vec())
            .collect()
    }

    #[test]
    fn receive_maps_known_mentions_and_drops_unknown() {
        let state = Arc::new(State::default());
        {
            let mut gets = state.gets.lock().unwrap();
            gets.push_back(resp(200, two_members()));
            gets.push_back(resp(
                200,
                json!({"value": [
                    message("m1", vec![mention("id-ada"), mention("id-bob"), mention("id-ada"), mention("stranger")], false),
                    message("m2", vec![mention("id-ada")], true),
                    message("m3", vec![], false),
                ]}),
            ));
        }
        let mut t = transport(&state, GraphCloud::China);
        let roster = members_projection(&open(&mut t));
        let (ada, bob) = (roster[0].unwrap(), roster[1].unwrap());
        let handles = read_history(&mut t, &chat());
        assert_eq!(handles[0], vec![ada, bob, ada]);
        assert!(handles[1].is_empty(), "a deleted message mentions no one");
        assert!(handles[2].is_empty());
    }

    #[test]
    fn mentions_are_capped_at_twenty() {
        let state = Arc::new(State::default());
        {
            let mut gets = state.gets.lock().unwrap();
            gets.push_back(resp(200, two_members()));
            gets.push_back(resp(
                200,
                json!({"value": [message("m1", vec![mention("id-ada"); 25], false)]}),
            ));
        }
        let mut t = transport(&state, GraphCloud::China);
        open(&mut t);
        assert_eq!(read_history(&mut t, &chat())[0].len(), 20);
    }

    #[test]
    fn history_of_another_chat_or_without_a_roster_resolves_nothing() {
        let state = Arc::new(State::default());
        {
            let mut gets = state.gets.lock().unwrap();
            gets.push_back(resp(200, two_members()));
            for _ in 0..2 {
                gets.push_back(resp(
                    200,
                    json!({"value": [message("m1", vec![mention("id-ada")], false)]}),
                ));
            }
        }
        let mut t = transport(&state, GraphCloud::China);
        open(&mut t);
        // Same member IDs, but this chat's roster was never read.
        let other = ProviderResourceRef::microsoft365("19:other@thread.v2");
        assert!(read_history(&mut t, &other)[0].is_empty());
        t.member_ids.clear();
        assert!(read_history(&mut t, &chat())[0].is_empty());
    }

    #[test]
    fn reauth_clears_directory_and_history_cache() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, two_members()));
        let mut t = transport(&state, GraphCloud::China);
        let roster = members_projection(&open(&mut t));
        t.chat_history_cache.seed_for_test("19:chat@thread.v2");
        assert!(t.chat_history_cache.contains_for_test("19:chat@thread.v2"));
        t.record_error(&TransportError::ReauthenticationRequired);
        assert_eq!(t.member_ids.len_for_test(), 0);
        assert!(!t.chat_history_cache.contains_for_test("19:chat@thread.v2"));
        assert_eq!(
            t.member_ids
                .resolve("19:chat@thread.v2", roster[0].unwrap()),
            None
        );
    }
}
