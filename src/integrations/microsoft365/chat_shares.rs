//! Giving a chat's other members read access to an uploaded file.
//!
//! A reference attachment is only a link: members who cannot open the file see
//! a message they cannot use. So, just before such a message is sent, the file
//! is shared with the chat, one job:
//!
//! 1. the chat's members are read once (the same bounded `members` read as the
//!    People panel, at most 100 rows), inside this job. The signed-in user and
//!    every row without a verified Entra user ID are left out. The IDs are used
//!    here and nowhere else: they never cross the seam, are never logged, and
//!    the receipt carries only counts;
//! 2. for each batch of 20 members, one `POST me/drive/items/{item}/invite` with
//!    `{"recipients": [{"objectId": ...}], "roles": ["read"], "requireSignIn":
//!    true, "sendInvitation": false}`. `sendInvitation: false` grants the
//!    permission directly and sends no email; `requireSignIn` keeps the access to
//!    signed-in members. `200` means every recipient was granted; `207` carries
//!    an `error` object on each recipient that was not. The `driveRecipient`
//!    takes exactly one of `email`, `alias` or `objectId`.
//!
//! A write: no request is ever replayed (a throttle stops the remaining
//! batches), and a failure never blocks the message, which is sent anyway; the
//! caller is told how many could not be given access so it can say so. See
//! Graph's [driveItem invite](https://learn.microsoft.com/en-us/graph/api/driveitem-invite)
//! and [driveRecipient](https://learn.microsoft.com/en-us/graph/api/resources/driverecipient).

use super::chat_members::{MembersReadFailure, fetch_chat_members};
use super::chat_uploads::UNAVAILABLE_MESSAGE;
use super::chats::selected_chat_identifier;
use super::mutation::is_dot_segment;
use super::text::normalized_identifier;
use super::{GraphHttpError, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::transport::{
    CommandDisposition, MAX_REMOTE_CHAT_MEMBERS, ShareReport, UploadReceipt, UploadedFileHandle,
};
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Recipients per `invite` request.
const INVITE_BATCH: usize = 20;
const MAX_INVITE_ANSWER_BYTES: u64 = 64 * 1024;

impl Microsoft365Transport {
    pub(super) fn share_uploaded_file(
        &mut self,
        chat: &ProviderResourceRef,
        upload: &UploadedFileHandle,
    ) -> Result<CommandReceipt, TransportError> {
        let chat_id = selected_chat_identifier(chat)?;
        // Only a file this session uploaded, and only its own drive item.
        let item_id = self
            .uploads
            .item_id(upload)
            .map(str::to_owned)
            .ok_or_else(|| {
                TransportError::InvalidCommand(
                    "the uploaded file is no longer available; attach it again".into(),
                )
            })?;
        if is_dot_segment(&item_id) {
            return Err(TransportError::InvalidCommand(
                "the uploaded file is not valid".into(),
            ));
        }

        let members = match fetch_chat_members(&self.session, chat_id, None) {
            Ok(members) => members,
            Err(MembersReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                return Err(error);
            }
            Err(failure) => return Err(failure.transport_error()),
        };
        let me = self
            .identity
            .as_ref()
            .map(|identity| identity.account_id.clone());
        let recipients = recipients_of(members.ids, me.as_deref());

        let url = self
            .session
            .cloud()
            .graph_route(&["me", "drive", "items", &item_id, "invite"], None)
            .map_err(|_| {
                TransportError::Unavailable(
                    "could not prepare the Microsoft Graph share route".into(),
                )
            })?;
        let mut report = ShareReport {
            granted: 0,
            failed: 0,
        };
        let mut stopped = false;
        for batch in recipients.chunks(INVITE_BATCH) {
            let batch_size = u32::try_from(batch.len()).unwrap_or(u32::MAX);
            if stopped {
                report.failed += batch_size;
                continue;
            }
            let body = serde_json::to_vec(&json!({
                "requireSignIn": true,
                "sendInvitation": false,
                "roles": ["read"],
                "recipients": batch.iter().map(|id| json!({"objectId": id})).collect::<Vec<_>>(),
            }))
            .map_err(|_| TransportError::Unavailable(UNAVAILABLE_MESSAGE.into()))?;
            let answer = self.session.post_json_once(
                &GraphJsonRequest {
                    url: &url,
                    content_type: "application/json",
                    body: &body,
                },
                MAX_INVITE_ANSWER_BYTES,
            );
            match answer {
                Ok(response) => match response.status {
                    200 => {
                        let failed = failed_in(&response.body).unwrap_or(0).min(batch_size);
                        report.failed += failed;
                        report.granted += batch_size - failed;
                    }
                    207 => {
                        // A partial success names each recipient that failed.
                        let failed = failed_in(&response.body)
                            .unwrap_or(batch_size)
                            .min(batch_size);
                        report.failed += failed;
                        report.granted += batch_size - failed;
                    }
                    401 => {
                        let error = TransportError::ReauthenticationRequired;
                        self.identity = None;
                        self.record_error(&error);
                        return Err(error);
                    }
                    403 if report.granted == 0 && report.failed == 0 => {
                        return Err(TransportError::Unauthorized);
                    }
                    429 if report.granted == 0 && report.failed == 0 => {
                        return Err(TransportError::Throttled {
                            retry_after_seconds: response.retry_after_seconds,
                        });
                    }
                    // Throttled or refused after earlier batches: the rest are
                    // not tried, and the caller is told how it went.
                    403 | 429 => {
                        report.failed += batch_size;
                        stopped = true;
                    }
                    _ => report.failed += batch_size,
                },
                Err(GraphHttpError::RenewedBeforeDelivery) => {
                    return Err(TransportError::InvalidCommand(
                        "Microsoft 365 renewed the session before sharing; send again".into(),
                    ));
                }
                Err(_) => report.failed += batch_size,
            }
        }
        Ok(CommandReceipt {
            command_id: "microsoft365:chat-file-share".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: Some(UploadReceipt::Shared(report)),
        })
    }
}

/// The directory IDs to invite: every verified member ID except the signed-in
/// user's own, once each, at most 100.
fn recipients_of(ids: Vec<Option<String>>, me: Option<&str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.into_iter()
        .flatten()
        .filter_map(|id| normalized_identifier(&id))
        .filter(|id| !is_dot_segment(id) && Some(id.as_str()) != me)
        .filter(|id| seen.insert(id.clone()))
        .take(MAX_REMOTE_CHAT_MEMBERS)
        .collect()
}

/// How many entries of an `invite` answer carry an `error`. `None` when the
/// answer cannot be read.
fn failed_in(body: &[u8]) -> Option<u32> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let entries = value.get("value")?.as_array()?;
    let failed = entries
        .iter()
        .filter(|entry| entry.get("error").is_some_and(|error| !error.is_null()))
        .count();
    u32::try_from(failed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphHttpResponse};
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, WorkspaceCommand, WorkspaceTransport,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    const GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";

    #[derive(Default)]
    struct State {
        gets: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        posts: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        urls: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Value>>,
        tokens: Mutex<Vec<String>>,
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
            self.0.urls.lock().unwrap().push(format!("GET {url}"));
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
            token: &str,
            _: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.0
                .urls
                .lock()
                .unwrap()
                .push(format!("POST {}", request.url));
            self.0.tokens.lock().unwrap().push(token.into());
            self.0
                .bodies
                .lock()
                .unwrap()
                .push(serde_json::from_slice(request.body).unwrap());
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
            retry_after_seconds: Some(4),
            body: body.to_string().into_bytes(),
        })
    }

    fn aad(name: &str, user: &str) -> Value {
        json!({"@odata.type": "#microsoft.graph.aadUserConversationMember",
               "displayName": name, "userId": user, "roles": []})
    }

    fn roster(ids: &[&str]) -> Value {
        json!({"value": ids.iter().map(|id| aad("Member", id)).collect::<Vec<_>>()})
    }

    fn transport(state: &Arc<State>) -> (Microsoft365Transport, UploadedFileHandle) {
        let mut transport = Microsoft365Transport::with_client_in(
            GraphCloud::Global,
            "sensitive-access-token",
            Box::new(Client(Arc::clone(state))),
        );
        let identity = AccountIdentity {
            account_id: "me-id".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        let id = transport.uploads.register_item(
            "q3.pdf".into(),
            GUID.into(),
            "https://contoso-my.sharepoint.com/personal/ada/q3.pdf".into(),
            "item/1".into(),
        );
        let handle = UploadedFileHandle::new(id, "q3.pdf".into(), 5, GUID.into(), false);
        (transport, handle)
    }

    fn chat() -> ProviderResourceRef {
        ProviderResourceRef::microsoft365("19:chat@thread.v2")
    }

    fn share(
        transport: &mut Microsoft365Transport,
        handle: &UploadedFileHandle,
    ) -> Result<CommandReceipt, TransportError> {
        transport.dispatch(WorkspaceCommand::ShareUploadedFileWithChat {
            chat: chat(),
            upload: handle.clone(),
        })
    }

    fn report(result: Result<CommandReceipt, TransportError>) -> ShareReport {
        match result.expect("a share").upload {
            Some(UploadReceipt::Shared(report)) => report,
            other => panic!("a share report, got {other:?}"),
        }
    }

    #[test]
    fn the_invite_body_is_read_only_signed_in_and_silent_and_names_members_by_object_id() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&["me-id", "id-bob", "id-eve"])));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": [{}, {}]})));
        let (mut t, handle) = transport(&state);
        assert_eq!(
            report(share(&mut t, &handle)),
            ShareReport {
                granted: 2,
                failed: 0
            }
        );
        let urls = state.urls.lock().unwrap().clone();
        assert!(
            urls[0].starts_with(
                "GET https://graph.microsoft.com/v1.0/chats/19:chat@thread.v2/members"
            )
        );
        assert_eq!(
            urls[1],
            "POST https://graph.microsoft.com/v1.0/me/drive/items/item%2F1/invite"
        );
        assert_eq!(
            state.bodies.lock().unwrap()[0],
            json!({
                "requireSignIn": true,
                "sendInvitation": false,
                "roles": ["read"],
                "recipients": [{"objectId": "id-bob"}, {"objectId": "id-eve"}]
            })
        );
        // Never `write`, never an email, never a notification.
        let body = state.bodies.lock().unwrap()[0].to_string();
        assert!(!body.contains("write") && !body.contains("email") && !body.contains("message"));
    }

    #[test]
    fn the_user_and_unverified_members_are_left_out_once_each() {
        let state = Arc::new(State::default());
        state.gets.lock().unwrap().push_back(resp(
            200,
            json!({"value": [
                aad("Me", "me-id"),
                aad("Bob", "id-bob"),
                aad("Bob again", "id-bob"),
                {"@odata.type": "#microsoft.graph.anonymousGuestConversationMember", "displayName": "G", "userId": "id-guest"},
                {"@odata.type": "#microsoft.graph.aadUserConversationMember", "displayName": "NoId"},
                aad("Dot", "..")
            ]}),
        ));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": [{}]})));
        let (mut t, handle) = transport(&state);
        assert_eq!(report(share(&mut t, &handle)).granted, 1);
        assert_eq!(
            state.bodies.lock().unwrap()[0]["recipients"],
            json!([{"objectId": "id-bob"}])
        );
    }

    #[test]
    fn a_chat_with_no_one_else_makes_no_invite_and_is_a_clean_report() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&["me-id"])));
        let (mut t, handle) = transport(&state);
        assert_eq!(
            report(share(&mut t, &handle)),
            ShareReport {
                granted: 0,
                failed: 0
            }
        );
        assert!(state.bodies.lock().unwrap().is_empty());
    }

    #[test]
    fn members_go_in_batches_of_twenty_and_a_207_counts_each_failed_recipient() {
        let state = Arc::new(State::default());
        let ids: Vec<String> = (0..45).map(|n| format!("id-{n}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&refs)));
        {
            let mut posts = state.posts.lock().unwrap();
            posts.push_back(resp(200, json!({"value": []})));
            // A partial success: two of the second batch have an error.
            posts.push_back(resp(
                207,
                json!({"value": [{"error": {"code": "notAllowed"}}, {}, {"error": {"code": "x"}}]}),
            ));
            posts.push_back(resp(200, json!({"value": []})));
        }
        let (mut t, handle) = transport(&state);
        let result = report(share(&mut t, &handle));
        let sizes: Vec<usize> = state
            .bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| body["recipients"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [20, 20, 5]);
        assert_eq!(
            result,
            ShareReport {
                granted: 43,
                failed: 2
            }
        );
    }

    #[test]
    fn an_unreadable_207_counts_the_whole_batch_failed_and_an_unreadable_200_counts_it_granted() {
        for (status, expected) in [
            (
                207,
                ShareReport {
                    granted: 0,
                    failed: 2,
                },
            ),
            (
                200,
                ShareReport {
                    granted: 2,
                    failed: 0,
                },
            ),
        ] {
            let state = Arc::new(State::default());
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, roster(&["id-a", "id-b"])));
            state
                .posts
                .lock()
                .unwrap()
                .push_back(resp(status, json!("not an object")));
            let (mut t, handle) = transport(&state);
            assert_eq!(report(share(&mut t, &handle)), expected, "{status}");
        }
    }

    #[test]
    fn first_batch_refusals_are_errors_and_nothing_is_replayed() {
        for (status, check) in [(403, 0), (429, 1), (401, 2)] {
            let state = Arc::new(State::default());
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, roster(&["id-a"])));
            state
                .posts
                .lock()
                .unwrap()
                .push_back(resp(status, json!({})));
            let (mut t, handle) = transport(&state);
            let error = share(&mut t, &handle).unwrap_err();
            match check {
                0 => assert_eq!(error, TransportError::Unauthorized),
                1 => assert_eq!(
                    error,
                    TransportError::Throttled {
                        retry_after_seconds: Some(4)
                    }
                ),
                _ => assert_eq!(error, TransportError::ReauthenticationRequired),
            }
            assert_eq!(state.bodies.lock().unwrap().len(), 1, "one POST ({status})");
        }
    }

    #[test]
    fn a_throttle_after_some_batches_stops_the_rest_and_reports_partial() {
        let state = Arc::new(State::default());
        let ids: Vec<String> = (0..45).map(|n| format!("id-{n}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&refs)));
        {
            let mut posts = state.posts.lock().unwrap();
            posts.push_back(resp(200, json!({"value": []})));
            posts.push_back(resp(429, json!({})));
        }
        let (mut t, handle) = transport(&state);
        assert_eq!(
            report(share(&mut t, &handle)),
            ShareReport {
                granted: 20,
                failed: 25
            }
        );
        assert_eq!(
            state.bodies.lock().unwrap().len(),
            2,
            "no third batch, no replay"
        );
    }

    #[test]
    fn other_failures_count_the_batch_failed_and_the_message_may_still_go() {
        for outcome in [
            resp(500, json!({})),
            resp(400, json!({})),
            Err(GraphHttpError::Network),
        ] {
            let state = Arc::new(State::default());
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(200, roster(&["id-a", "id-b"])));
            state.posts.lock().unwrap().push_back(outcome);
            let (mut t, handle) = transport(&state);
            assert_eq!(
                report(share(&mut t, &handle)),
                ShareReport {
                    granted: 0,
                    failed: 2
                }
            );
        }
    }

    #[test]
    fn a_member_list_that_cannot_be_read_shares_nothing() {
        for (status, check) in [(403, 0), (429, 1), (500, 2)] {
            let state = Arc::new(State::default());
            state
                .gets
                .lock()
                .unwrap()
                .push_back(resp(status, json!({})));
            let (mut t, handle) = transport(&state);
            let error = share(&mut t, &handle).unwrap_err();
            match check {
                0 => assert_eq!(error, TransportError::Unauthorized),
                1 => assert!(matches!(error, TransportError::Throttled { .. })),
                _ => assert!(matches!(error, TransportError::Unavailable(_))),
            }
            assert!(state.bodies.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn only_a_file_this_session_uploaded_can_be_shared() {
        let state = Arc::new(State::default());
        let (mut t, handle) = transport(&state);
        let stranger =
            UploadedFileHandle::new(handle.id() ^ 1, "q3.pdf".into(), 5, GUID.into(), false);
        let wrong_reference = UploadedFileHandle::new(
            handle.id(),
            "q3.pdf".into(),
            5,
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".into(),
            false,
        );
        for bad in [stranger, wrong_reference] {
            assert!(matches!(
                share(&mut t, &bad),
                Err(TransportError::InvalidCommand(_))
            ));
        }
        t.record_error(&TransportError::ReauthenticationRequired);
        assert!(matches!(
            share(&mut t, &handle),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.urls.lock().unwrap().is_empty(), "no request was made");
    }

    #[test]
    fn a_local_chat_is_refused_and_member_ids_never_appear_in_debug_output() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&["id-secret-member"])));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": [{}]})));
        let (mut t, handle) = transport(&state);
        let local = t.dispatch(WorkspaceCommand::ShareUploadedFileWithChat {
            chat: ProviderResourceRef::local_preview("local"),
            upload: handle.clone(),
        });
        assert!(local.is_err());
        let receipt = share(&mut t, &handle).unwrap();
        let debug = format!(
            "{receipt:?} {:?}",
            ShareReport {
                granted: 1,
                failed: 0
            }
        );
        assert!(!debug.contains("id-secret-member"));
        assert!(!format!("{t_uploads:?}", t_uploads = t.uploads).contains("item/1"));
    }

    #[test]
    fn the_bearer_token_goes_only_to_graph() {
        let state = Arc::new(State::default());
        state
            .gets
            .lock()
            .unwrap()
            .push_back(resp(200, roster(&["id-a"])));
        state
            .posts
            .lock()
            .unwrap()
            .push_back(resp(200, json!({"value": [{}]})));
        let (mut t, handle) = transport(&state);
        share(&mut t, &handle).unwrap();
        for url in state.urls.lock().unwrap().iter() {
            assert!(url.contains("https://graph.microsoft.com/"), "{url}");
        }
        assert_eq!(
            state.tokens.lock().unwrap().as_slice(),
            ["sensitive-access-token"]
        );
    }

    #[test]
    fn recipients_are_deduplicated_and_capped_at_a_hundred() {
        let ids: Vec<Option<String>> = (0..150).map(|n| Some(format!("id-{}", n % 120))).collect();
        let recipients = recipients_of(ids, Some("id-3"));
        assert_eq!(recipients.len(), 100);
        assert!(!recipients.contains(&"id-3".to_owned()));
    }
}
