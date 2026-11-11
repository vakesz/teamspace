//! Microsoft Graph opt-in read receipts for one Microsoft 365 chat.
//!
//! `POST /chats/{chat}/markChatReadForUser` with the user and tenant IDs in
//! the body. It needs the optional delegated `Chat.ReadWrite` scope, works
//! in every cloud, accepts only `204 No Content`, and is never replayed or
//! reconciled: a failed receipt is simply not sent again for that view.
//!
//! The tenant ID is looked up lazily, once per adapter, from `/organization`
//! (`User.Read`), with the signed-in user's own chat membership as a fallback,
//! and lives only in the adapter.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segments};
use super::text::normalized_identifier;
use super::{GraphJsonRequest, Microsoft365Transport};
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use serde::Serialize;
use serde_json::Value;
use url::Url;

const MAX_LOOKUP_RESPONSE_BYTES: u64 = 256 * 1024;

impl Microsoft365Transport {
    pub(super) fn mark_chat_read(
        &mut self,
        chat: &ProviderResourceRef,
    ) -> Result<CommandReceipt, TransportError> {
        let chat_id = chat.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
        })?;
        // No verified identity (or tenant) means there is no safe route or
        // body to build: make no request at all.
        let account_id = self
            .identity
            .as_ref()
            .map(|identity| identity.account_id.clone())
            .ok_or_else(|| {
                TransportError::Unavailable(
                    "Microsoft Graph needs the verified account before sending a read receipt"
                        .into(),
                )
            })?;
        reject_dot_segments(&[chat_id])?;
        let tenant_id = self.tenant_id_for(chat_id, &account_id)?;
        let url = self
            .session
            .cloud()
            .graph_route(&["chats", chat_id, "markChatReadForUser"], None)
            .map_err(|_| {
                TransportError::Unavailable(
                    "could not prepare the Microsoft Graph read receipt route".into(),
                )
            })?;
        let body = serde_json::to_vec(&ReadReceiptPayload {
            user: ReadReceiptUser {
                id: &account_id,
                tenant_id: &tenant_id,
            },
        })
        .map_err(|_| {
            TransportError::Unavailable("could not prepare the read receipt for delivery".into())
        })?;
        self.session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: &body,
            })
            .map_err(|error| {
                map_mutation_http_error(
                    error,
                    "Microsoft Graph could not confirm the read receipt",
                    "Microsoft Graph read receipts are unavailable",
                )
            })
            .and_then(|response| {
                self.classify_mutation_status(
                    response,
                    204,
                    "microsoft365:chat-read-receipt",
                    MutationMessages {
                        rejected: "Microsoft Graph rejected the read receipt",
                        unconfirmed: "Microsoft Graph could not confirm the read receipt",
                    },
                )
            })
    }

    /// The signed-in user's tenant, from the adapter cache, else one
    /// `/organization` read, else the user's own membership of this chat.
    fn tenant_id_for(&mut self, chat_id: &str, account_id: &str) -> Result<String, TransportError> {
        if let Some(tenant) = &self.tenant_id {
            return Ok(tenant.clone());
        }
        let found = match self.read_organization_tenant()? {
            Some(tenant) => Some(tenant),
            None => self.read_member_tenant(chat_id, account_id)?,
        };
        let tenant = found.ok_or_else(|| {
            TransportError::Unavailable(
                "Microsoft Graph did not report the tenant needed for a read receipt".into(),
            )
        })?;
        self.tenant_id = Some(tenant.clone());
        Ok(tenant)
    }

    fn read_organization_tenant(&mut self) -> Result<Option<String>, TransportError> {
        let Ok(url) = self
            .session
            .cloud()
            .graph_route(&["organization"], Some("$select=id"))
        else {
            return Ok(None);
        };
        self.lookup(&url, |body| {
            body.get("value")?
                .as_array()?
                .first()?
                .get("id")?
                .as_str()
                .and_then(normalized_identifier)
        })
    }

    fn read_member_tenant(
        &mut self,
        chat_id: &str,
        account_id: &str,
    ) -> Result<Option<String>, TransportError> {
        let Ok(url) = self
            .session
            .cloud()
            .graph_route(&["chats", chat_id, "members"], None)
        else {
            return Ok(None);
        };
        self.lookup(&url, |body| {
            body.get("value")?.as_array()?.iter().find_map(|member| {
                (member.get("userId")?.as_str()? == account_id)
                    .then(|| member.get("tenantId")?.as_str())
                    .flatten()
                    .and_then(normalized_identifier)
            })
        })
    }

    /// One bounded read. A `401` needs a new sign-in; any other failure just
    /// means this source did not answer (the caller falls back or declines).
    fn lookup(
        &mut self,
        url: &Url,
        extract: impl FnOnce(&Value) -> Option<String>,
    ) -> Result<Option<String>, TransportError> {
        let Ok(response) = self.session.get_json(url, MAX_LOOKUP_RESPONSE_BYTES) else {
            return Ok(None);
        };
        match response.status {
            200 => Ok(serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|body| extract(&body))),
            401 => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            _ => Ok(None),
        }
    }
}

#[derive(Serialize)]
struct ReadReceiptPayload<'a> {
    user: ReadReceiptUser<'a>,
}

#[derive(Serialize)]
struct ReadReceiptUser<'a> {
    id: &'a str,
    #[serde(rename = "tenantId")]
    tenant_id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::super::{
        GraphHttpClient, GraphHttpError, GraphHttpResponse, GraphHttpStatus, GraphJsonRequest,
        Microsoft365Transport,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::{
        AccountIdentity, CommandDisposition, ConnectionStatus, ProviderResourceRef, TransportError,
        WorkspaceCommand, WorkspaceTransport,
    };
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    struct State {
        gets: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        posts: Mutex<VecDeque<Result<GraphHttpStatus, GraphHttpError>>>,
        sent: Mutex<Vec<(String, String, Vec<u8>)>>,
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
            self.0
                .sent
                .lock()
                .expect("sent")
                .push(("GET".into(), url.as_str().into(), Vec::new()));
            self.0
                .gets
                .lock()
                .expect("gets")
                .pop_front()
                .expect("a GET response must be configured")
        }

        fn post_json_status(
            &self,
            request: GraphJsonRequest<'_>,
            _: &str,
        ) -> Result<GraphHttpStatus, GraphHttpError> {
            self.0.sent.lock().expect("sent").push((
                "POST".into(),
                request.url.as_str().into(),
                request.body.to_vec(),
            ));
            self.0
                .posts
                .lock()
                .expect("posts")
                .pop_front()
                .expect("a POST response must be configured")
        }
    }

    fn ok(body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        reply(200, body)
    }

    fn reply(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn status(status: u16) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status,
            retry_after_seconds: None,
        })
    }

    fn transport(
        cloud: GraphCloud,
        identity: bool,
        gets: Vec<Result<GraphHttpResponse, GraphHttpError>>,
        posts: Vec<Result<GraphHttpStatus, GraphHttpError>>,
    ) -> (Microsoft365Transport, Arc<State>) {
        let state = Arc::new(State {
            gets: Mutex::new(gets.into()),
            posts: Mutex::new(posts.into()),
            sent: Mutex::new(Vec::new()),
        });
        let mut transport = Microsoft365Transport::with_client_in(
            cloud,
            "sensitive-access-token",
            Box::new(Client(Arc::clone(&state))),
        );
        if identity {
            let identity = AccountIdentity {
                account_id: "acct-1".into(),
                display_name: "Ada".into(),
                email: None,
                tenant_id: None,
            };
            transport.identity = Some(identity.clone());
            transport.status = ConnectionStatus::Connected(identity);
        }
        (transport, state)
    }

    fn mark(chat: &str) -> WorkspaceCommand {
        WorkspaceCommand::MarkChatRead {
            chat: ProviderResourceRef::microsoft365(chat),
        }
    }

    fn path(url: &str) -> Vec<String> {
        Url::parse(url)
            .expect("url")
            .path_segments()
            .expect("segments")
            .map(str::to_owned)
            .collect()
    }

    const ORG: &str = r#"{"value":[{"id":"tenant-9"}]}"#;

    #[test]
    fn posts_the_exact_route_and_body_and_204_confirms() {
        let (mut transport, state) =
            transport(GraphCloud::Global, true, vec![ok(ORG)], vec![status(204)]);
        let receipt = transport
            .dispatch(mark("19:chat id@thread.v2"))
            .expect("204 confirms");
        assert_eq!(receipt.command_id, "microsoft365:chat-read-receipt");
        assert_eq!(receipt.disposition, CommandDisposition::AppliedRemotely);
        let sent = state.sent.lock().expect("sent");
        assert_eq!(sent.len(), 2);
        assert_eq!(
            path(&sent[0].1),
            ["v1.0", "organization"],
            "tenant lookup is the first request"
        );
        let (method, url, body) = &sent[1];
        assert_eq!(method, "POST");
        assert_eq!(
            path(url),
            [
                "v1.0",
                "chats",
                "19:chat%20id@thread.v2",
                "markChatReadForUser"
            ]
        );
        let body: Value = serde_json::from_slice(body).expect("json");
        assert_eq!(
            body,
            serde_json::json!({"user":{"id":"acct-1","tenantId":"tenant-9"}})
        );
    }

    #[test]
    fn it_works_in_every_cloud_with_that_clouds_host() {
        for cloud in [
            GraphCloud::Global,
            GraphCloud::UsGov,
            GraphCloud::UsGovDod,
            GraphCloud::China,
        ] {
            let (mut transport, state) = transport(cloud, true, vec![ok(ORG)], vec![status(204)]);
            transport
                .dispatch(mark("c"))
                .expect("every cloud allows it");
            let sent = state.sent.lock().expect("sent");
            for (_, url, _) in sent.iter() {
                let host = Url::parse(url).expect("url").host_str().map(str::to_owned);
                assert_eq!(host.as_deref(), Some(cloud.graph_host()), "{cloud:?}");
            }
        }
    }

    #[test]
    fn only_204_confirms_and_the_status_matrix_is_classified() {
        for (code, expect) in [
            (200, "uncertain"),
            (202, "uncertain"),
            (400, "rejected"),
            (404, "rejected"),
            (403, "unauthorized"),
            (429, "throttled"),
            (500, "uncertain"),
        ] {
            let (mut transport, _) =
                transport(GraphCloud::Global, true, vec![ok(ORG)], vec![status(code)]);
            let actual = match transport.dispatch(mark("c")).expect_err("not a receipt") {
                TransportError::InvalidCommand(_) => "rejected",
                TransportError::Unauthorized => "unauthorized",
                TransportError::Throttled { .. } => "throttled",
                TransportError::Unavailable(_) => "uncertain",
                other => panic!("{code}: {other:?}"),
            };
            assert_eq!(actual, expect, "{code}");
        }
        let (mut transport, _) = transport(
            GraphCloud::Global,
            true,
            vec![ok(ORG)],
            vec![Err(GraphHttpError::Network)],
        );
        assert!(matches!(
            transport.dispatch(mark("c")),
            Err(TransportError::Unavailable(_))
        ));
    }

    #[test]
    fn no_verified_identity_makes_no_request() {
        let (mut transport, state) = transport(GraphCloud::Global, false, vec![], vec![]);
        assert!(matches!(
            transport.dispatch(mark("c")),
            Err(TransportError::Unavailable(_))
        ));
        assert!(state.sent.lock().expect("sent").is_empty());
    }

    #[test]
    fn no_tenant_anywhere_makes_no_post() {
        // The organization answers without an id and the membership read
        // does not list the user.
        let (mut transport, state) = transport(
            GraphCloud::Global,
            true,
            vec![
                ok(r#"{"value":[]}"#),
                ok(r#"{"value":[{"userId":"other","tenantId":"t"}]}"#),
            ],
            vec![],
        );
        assert!(matches!(
            transport.dispatch(mark("c")),
            Err(TransportError::Unavailable(_))
        ));
        let sent = state.sent.lock().expect("sent");
        assert!(sent.iter().all(|(method, ..)| method == "GET"));
    }

    #[test]
    fn the_own_membership_is_the_tenant_fallback() {
        let (mut transport, state) = transport(
            GraphCloud::Global,
            true,
            vec![
                reply(403, "{}"),
                ok(
                    r#"{"value":[{"userId":"x","tenantId":"no"},{"userId":"acct-1","tenantId":"tenant-m"}]}"#,
                ),
            ],
            vec![status(204)],
        );
        transport.dispatch(mark("c")).expect("fallback tenant");
        let sent = state.sent.lock().expect("sent");
        assert_eq!(path(&sent[1].1), ["v1.0", "chats", "c", "members"]);
        let body: Value = serde_json::from_slice(&sent[2].2).expect("json");
        assert_eq!(body["user"]["tenantId"], "tenant-m");
    }

    #[test]
    fn the_organization_read_is_cached_for_the_adapter() {
        let (mut transport, state) = transport(
            GraphCloud::Global,
            true,
            vec![ok(ORG)],
            vec![status(204), status(204), status(204)],
        );
        for _ in 0..3 {
            transport.dispatch(mark("c")).expect("receipt");
        }
        let sent = state.sent.lock().expect("sent");
        assert_eq!(
            sent.iter().filter(|(method, ..)| method == "GET").count(),
            1
        );
        assert_eq!(
            sent.iter().filter(|(method, ..)| method == "POST").count(),
            3
        );
    }

    #[test]
    fn a_401_needs_reauthentication_and_never_replays() {
        let (mut transport, state) =
            transport(GraphCloud::Global, true, vec![ok(ORG)], vec![status(401)]);
        assert!(matches!(
            transport.dispatch(mark("c")),
            Err(TransportError::ReauthenticationRequired)
        ));
        let posts = state
            .sent
            .lock()
            .expect("sent")
            .iter()
            .filter(|(method, ..)| method == "POST")
            .count();
        assert_eq!(posts, 1);
    }

    #[test]
    fn dot_segments_and_local_targets_make_no_request() {
        let (mut transport, state) = transport(GraphCloud::Global, true, vec![], vec![]);
        assert!(transport.dispatch(mark("..")).is_err());
        let local = WorkspaceCommand::MarkChatRead {
            chat: ProviderResourceRef::local_preview("fixture-chat"),
        };
        assert!(transport.dispatch(local).is_err());
        assert!(state.sent.lock().expect("sent").is_empty());
    }
}
