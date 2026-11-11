//! Microsoft Graph edit and soft-delete of the signed-in user's own chat
//! messages.
//!
//! Both are deliberately separate from sends: they need the optional, broader
//! delegated `Chat.ReadWrite` scope, accept only `204 No Content`, add no
//! optimistic row, and are never replayed. Editing is documented for the
//! global cloud only; soft-delete is available in every cloud.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segments};
use super::rich_html::{MentionScope, wire_body};
use super::{GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::{CommandReceipt, ComposedBody, ProviderResourceRef, TransportError};
use serde::Serialize;
use url::Url;
use zeroize::Zeroizing;

impl Microsoft365Transport {
    /// Replaces the plain-text body of one own message with a `PATCH`.
    pub(super) fn edit_chat_message(
        &mut self,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
        body: &ComposedBody,
    ) -> Result<CommandReceipt, TransportError> {
        let (chat_id, message_id) = remote_ids(chat, message)?;
        // Graph documents the update only for the global cloud; refuse
        // before any request or token use elsewhere.
        if !self.session.cloud().supports_chat_message_update() {
            return Err(TransportError::Unsupported(
                "Microsoft Graph does not support editing chat messages in this cloud".into(),
            ));
        }
        let wire = wire_body(body, "message edit body", &MentionScope::None)?;
        let url = message_url(self.session.cloud(), chat_id, message_id, None)?;
        let payload = EditPayload {
            body: EditBody {
                content_type: wire.content_type,
                content: &wire.content,
            },
        };
        let request_body = Zeroizing::new(serde_json::to_vec(&payload).map_err(|_| {
            TransportError::Unavailable("could not prepare the message edit for delivery".into())
        })?);
        let result = self
            .session
            .patch_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(|error| {
                map_mutation_http_error(
                    error,
                    "Microsoft Graph could not confirm the message edit",
                    "Microsoft Graph message edits are unavailable",
                )
            })
            .and_then(|response| {
                self.classify_mutation_status(
                    response,
                    204,
                    "microsoft365:chat-message-edit",
                    MutationMessages {
                        rejected: "Microsoft Graph rejected the message edit",
                        unconfirmed: "Microsoft Graph could not confirm the message edit",
                    },
                )
            });
        self.invalidate_history_after(chat_id, &result);
        result
    }

    /// Soft-deletes one own message with a body-less `POST`.
    pub(super) fn delete_chat_message(
        &mut self,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
    ) -> Result<CommandReceipt, TransportError> {
        let (chat_id, message_id) = remote_ids(chat, message)?;
        // The route embeds the signed-in user's ID; without a verified
        // identity there is no safe route to build.
        let account_id = self
            .identity
            .as_ref()
            .map(|identity| identity.account_id.clone())
            .ok_or_else(|| {
                TransportError::Unavailable(
                    "Microsoft Graph needs the verified account before deleting a message".into(),
                )
            })?;
        let url = message_url(self.session.cloud(), chat_id, message_id, Some(&account_id))?;
        let result = self
            .session
            .post_empty_status(&url)
            .map_err(|error| {
                map_mutation_http_error(
                    error,
                    "Microsoft Graph could not confirm the message deletion",
                    "Microsoft Graph message deletion is unavailable",
                )
            })
            .and_then(|response| {
                self.classify_mutation_status(
                    response,
                    204,
                    "microsoft365:chat-message-delete",
                    MutationMessages {
                        rejected: "Microsoft Graph rejected the message deletion",
                        unconfirmed: "Microsoft Graph could not confirm the message deletion",
                    },
                )
            });
        self.invalidate_history_after(chat_id, &result);
        result
    }

    /// A success or an ambiguous outcome may have changed the chat; a known
    /// rejection or pre-network failure did not.
    fn invalidate_history_after(
        &mut self,
        chat_id: &str,
        result: &Result<CommandReceipt, TransportError>,
    ) {
        if matches!(result, Ok(_) | Err(TransportError::Unavailable(_))) {
            self.chat_history_cache.invalidate(chat_id);
        }
    }
}

fn remote_ids<'a>(
    chat: &'a ProviderResourceRef,
    message: &'a ProviderResourceRef,
) -> Result<(&'a str, &'a str), TransportError> {
    let chat_id = chat.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
    })?;
    let message_id = message.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported(
            "Microsoft Graph cannot target a local preview chat message".into(),
        )
    })?;
    Ok((chat_id, message_id))
}

/// `account_id` selects the delete route
/// `/users/{id}/chats/{chat}/messages/{message}/softDelete`; `None` selects the
/// edit route `/chats/{chat}/messages/{message}`.
fn message_url(
    cloud: GraphCloud,
    chat_id: &str,
    message_id: &str,
    account_id: Option<&str>,
) -> Result<Url, TransportError> {
    // `url` normalizes dot segments; a provider ID must not retarget this
    // token-bearing route through `.` or `..`.
    reject_dot_segments(&[chat_id, message_id, account_id.unwrap_or("x")])?;
    let route = match account_id {
        Some(account) => cloud.graph_route(
            &[
                "users",
                account,
                "chats",
                chat_id,
                "messages",
                message_id,
                "softDelete",
            ],
            None,
        ),
        None => cloud.graph_route(&["chats", chat_id, "messages", message_id], None),
    };
    route.map_err(|_| {
        TransportError::Unavailable("could not prepare the Microsoft Graph message route".into())
    })
}

#[derive(Serialize)]
struct EditPayload<'a> {
    body: EditBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EditBody<'a> {
    content_type: &'static str,
    content: &'a str,
}

#[cfg(test)]
mod tests {
    use super::message_url;
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::rich_html::MAX_REMOTE_BODY_BYTES as MAX_REMOTE_EDIT_BYTES;
    use crate::integrations::microsoft365::{
        GraphHttpClient, GraphHttpError, GraphHttpResponse, GraphHttpStatus, GraphJsonRequest,
        Microsoft365Transport,
    };
    use crate::integrations::{
        AccountIdentity, CommandDisposition, ConnectionStatus, ProviderResourceRef, TransportError,
        WorkspaceCommand, WorkspaceTransport,
    };
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    #[derive(Debug)]
    struct Recorded {
        method: &'static str,
        url: String,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    struct State {
        responses: Mutex<VecDeque<Result<GraphHttpStatus, GraphHttpError>>>,
        sent: Mutex<Vec<Recorded>>,
    }

    struct Client(Arc<State>);

    impl Client {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpStatus, GraphHttpError>>,
        ) -> (Self, Arc<State>) {
            let state = Arc::new(State {
                responses: Mutex::new(responses.into_iter().collect()),
                sent: Mutex::new(Vec::new()),
            });
            (Self(Arc::clone(&state)), state)
        }

        fn record(
            &self,
            method: &'static str,
            url: &Url,
            content_type: Option<&str>,
            body: &[u8],
        ) -> Result<GraphHttpStatus, GraphHttpError> {
            self.0.sent.lock().expect("sent").push(Recorded {
                method,
                url: url.as_str().into(),
                content_type: content_type.map(str::to_owned),
                body: body.to_vec(),
            });
            self.0
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    impl GraphHttpClient for Client {
        fn get_me(&self, _: &Url, _: &str) -> Result<GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn patch_json_status(
            &self,
            request: GraphJsonRequest<'_>,
            _: &str,
        ) -> Result<GraphHttpStatus, GraphHttpError> {
            self.record(
                "PATCH",
                request.url,
                Some(request.content_type),
                request.body,
            )
        }

        fn post_empty_status(&self, url: &Url, _: &str) -> Result<GraphHttpStatus, GraphHttpError> {
            self.record("POST", url, None, &[])
        }

        fn post_json_status(
            &self,
            _: GraphJsonRequest<'_>,
            _: &str,
        ) -> Result<GraphHttpStatus, GraphHttpError> {
            panic!("edit and delete must never use the JSON POST path");
        }
    }

    fn status(status: u16) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status,
            retry_after_seconds: None,
        })
    }

    fn transport_in(cloud: GraphCloud, identity: bool, client: Client) -> Microsoft365Transport {
        let mut transport = Microsoft365Transport::with_client_in(
            cloud,
            "sensitive-access-token",
            Box::new(client),
        );
        if identity {
            let identity = AccountIdentity {
                account_id: "acct 1".into(),
                display_name: "Ada".into(),
                email: None,
                tenant_id: None,
            };
            transport.identity = Some(identity.clone());
            transport.status = ConnectionStatus::Connected(identity);
        }
        transport
    }

    fn edit(chat: &str, message: &str, body: &str) -> WorkspaceCommand {
        WorkspaceCommand::EditChatMessage {
            chat: ProviderResourceRef::microsoft365(chat),
            message: ProviderResourceRef::microsoft365(message),
            body: body.into(),
        }
    }

    fn delete(chat: &str, message: &str) -> WorkspaceCommand {
        WorkspaceCommand::DeleteChatMessage {
            chat: ProviderResourceRef::microsoft365(chat),
            message: ProviderResourceRef::microsoft365(message),
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

    #[test]
    fn edit_patches_the_exact_route_with_a_text_body() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        let receipt = transport
            .dispatch(edit("19:chat id@thread.v2", "msg/1", "new text"))
            .expect("204 acknowledges an edit");
        assert_eq!(receipt.command_id, "microsoft365:chat-message-edit");
        assert_eq!(receipt.disposition, CommandDisposition::AppliedRemotely);
        let sent = state.sent.lock().expect("sent");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].method, "PATCH");
        assert_eq!(
            path(&sent[0].url),
            [
                "v1.0",
                "chats",
                "19:chat%20id@thread.v2",
                "messages",
                "msg%2F1"
            ]
        );
        assert_eq!(sent[0].content_type.as_deref(), Some("application/json"));
        let body: Value = serde_json::from_slice(&sent[0].body).expect("json");
        assert_eq!(
            body,
            serde_json::json!({"body":{"contentType":"text","content":"new text"}})
        );
    }

    #[test]
    fn a_formatted_edit_body_is_sent_as_allow_listed_html() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        transport
            .dispatch(WorkspaceCommand::EditChatMessage {
                chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
                message: ProviderResourceRef::microsoft365("msg1"),
                body: crate::integrations::ComposedBody::from_markdown_lite("_new_ <i>text</i>"),
            })
            .expect("204 acknowledges an edit");
        let sent = state.sent.lock().expect("sent");
        let body: Value = serde_json::from_slice(&sent[0].body).expect("json");
        assert_eq!(
            body,
            serde_json::json!({"body":{"contentType":"html","content":"<p><em>new</em> &lt;i&gt;text&lt;/i&gt;</p>"}})
        );
    }

    #[test]
    fn edit_with_mention_is_rejected() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        let handles = transport.member_ids.remember_roster(
            "19:chat@thread.v2",
            vec![(Some("id-bob".into()), "Bob".into())],
        );
        let picked = [crate::integrations::DraftMention {
            handle: handles[0].expect("handle"),
            display: "Bob".into(),
        }];
        let error = transport
            .dispatch(WorkspaceCommand::EditChatMessage {
                chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
                message: ProviderResourceRef::microsoft365("msg1"),
                body: crate::integrations::ComposedBody::compose("@Bob hi", &picked),
            })
            .expect_err("an edit cannot carry a mention");
        assert!(matches!(error, TransportError::InvalidCommand(_)));
        assert!(state.sent.lock().expect("sent").is_empty());
    }

    #[test]
    fn delete_posts_an_empty_soft_delete_to_the_users_route() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        let receipt = transport
            .dispatch(delete("19:chat@thread.v2", "msg1"))
            .expect("204 acknowledges a deletion");
        assert_eq!(receipt.command_id, "microsoft365:chat-message-delete");
        let sent = state.sent.lock().expect("sent");
        assert_eq!(sent[0].method, "POST");
        assert_eq!(
            path(&sent[0].url),
            [
                "v1.0",
                "users",
                "acct%201",
                "chats",
                "19:chat@thread.v2",
                "messages",
                "msg1",
                "softDelete"
            ]
        );
        assert!(sent[0].content_type.is_none());
        assert!(sent[0].body.is_empty());
    }

    #[test]
    fn only_204_confirms_and_the_status_matrix_is_classified() {
        for command in [edit("c", "m", "x"), delete("c", "m")] {
            for (code, expect) in [
                (200, "uncertain"),
                (201, "uncertain"),
                (202, "uncertain"),
                (400, "rejected"),
                (404, "rejected"),
                (409, "rejected"),
                (413, "rejected"),
                (422, "rejected"),
                (403, "unauthorized"),
                (429, "throttled"),
                (500, "uncertain"),
                (503, "uncertain"),
            ] {
                let (client, _) = Client::new([status(code)]);
                let mut transport = transport_in(GraphCloud::Global, true, client);
                let error = transport
                    .dispatch(command.clone())
                    .expect_err("non-204 is never a receipt");
                let actual = match error {
                    TransportError::InvalidCommand(_) => "rejected",
                    TransportError::Unauthorized => "unauthorized",
                    TransportError::Throttled { .. } => "throttled",
                    TransportError::Unavailable(_) => "uncertain",
                    other => panic!("{code}: {other:?}"),
                };
                assert_eq!(actual, expect, "{command:?} {code}");
            }
            let (client, _) = Client::new([Err(GraphHttpError::Network)]);
            let mut transport = transport_in(GraphCloud::Global, true, client);
            assert!(matches!(
                transport.dispatch(command),
                Err(TransportError::Unavailable(_))
            ));
        }
    }

    #[test]
    fn a_401_without_a_refresher_requires_reauthentication() {
        let (client, _) = Client::new([status(401)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        assert!(matches!(
            transport.dispatch(delete("c", "m")),
            Err(TransportError::ReauthenticationRequired)
        ));
    }

    #[test]
    fn delete_without_a_verified_identity_makes_no_request() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, false, client);
        assert!(matches!(
            transport.dispatch(delete("c", "m")),
            Err(TransportError::Unavailable(_))
        ));
        assert!(state.sent.lock().expect("sent").is_empty());
    }

    #[test]
    fn edit_outside_global_is_unsupported_before_any_request() {
        for cloud in GraphCloud::ALL {
            if cloud == GraphCloud::Global {
                continue;
            }
            let (client, state) = Client::new([status(204)]);
            let mut transport = transport_in(cloud, true, client);
            assert!(matches!(
                transport.dispatch(edit("c", "m", "x")),
                Err(TransportError::Unsupported(_))
            ));
            assert!(state.sent.lock().expect("sent").is_empty());
            // Deletion works in every cloud and uses that cloud's host.
            let (client, state) = Client::new([status(204)]);
            let mut transport = transport_in(cloud, true, client);
            transport.dispatch(delete("c", "m")).expect("delete");
            let sent = state.sent.lock().expect("sent");
            assert_eq!(
                Url::parse(&sent[0].url).expect("url").host_str(),
                Some(cloud.graph_host())
            );
        }
    }

    #[test]
    fn dot_segments_and_bad_bodies_make_no_request() {
        let (client, state) = Client::new([status(204), status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        for command in [
            edit("..", "m", "x"),
            edit("c", " . ", "x"),
            delete("c", ".."),
            delete("..", "m"),
        ] {
            assert!(matches!(
                transport.dispatch(command),
                Err(TransportError::Unsupported(_))
            ));
        }
        for body in ["", "   ", "a\0b"] {
            assert!(matches!(
                transport.dispatch(edit("c", "m", body)),
                Err(TransportError::InvalidCommand(_))
            ));
        }
        let long = "x".repeat(MAX_REMOTE_EDIT_BYTES + 1);
        assert!(matches!(
            transport.dispatch(edit("c", "m", &long)),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.sent.lock().expect("sent").is_empty());
        assert!(message_url(GraphCloud::Global, "c", "m", Some("..")).is_err());
    }

    #[test]
    fn local_preview_targets_are_rejected_before_any_request() {
        let (client, state) = Client::new([status(204)]);
        let mut transport = transport_in(GraphCloud::Global, true, client);
        let local = WorkspaceCommand::DeleteChatMessage {
            chat: ProviderResourceRef::local_preview("fixture"),
            message: ProviderResourceRef::local_preview("fixture"),
        };
        let mixed = WorkspaceCommand::EditChatMessage {
            chat: ProviderResourceRef::microsoft365("c"),
            message: ProviderResourceRef::local_preview("m"),
            body: "x".into(),
        };
        for command in [local, mixed] {
            assert!(transport.dispatch(command).is_err());
        }
        assert!(state.sent.lock().expect("sent").is_empty());
    }

    #[test]
    fn success_and_ambiguity_invalidate_only_that_chats_history_cache() {
        for (response, invalidated) in [
            (status(204), true),
            (Err(GraphHttpError::Network), true),
            (status(400), false),
            (status(403), false),
        ] {
            let (client, _) = Client::new([response]);
            let mut transport = transport_in(GraphCloud::Global, true, client);
            transport.chat_history_cache.seed_for_test("chat-a");
            transport.chat_history_cache.seed_for_test("chat-b");
            let _ = transport.dispatch(delete("chat-a", "m"));
            assert_eq!(
                transport.chat_history_cache.contains_for_test("chat-a"),
                !invalidated
            );
            assert!(transport.chat_history_cache.contains_for_test("chat-b"));
        }
    }
}
