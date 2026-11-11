//! Microsoft Graph selected-chat quote-reply delivery.
//!
//! This adapter owns the one-message `replyWithQuote` route. It deliberately
//! keeps Graph's documented content-only reply payload separate from ordinary
//! chat sends: the endpoint documentation does not establish that an
//! `importance` field is accepted, so quote replies use the Normal-priority
//! first slice and never inherit the generic-send wire shape.

use super::mutation::{map_mutation_http_error, reject_dot_segment};
use super::{GraphHttpError, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::MAX_REMOTE_QUOTE_SOURCES;
use crate::integrations::{ProviderResourceRef, TransportError};
use serde::Serialize;
use url::Url;
use zeroize::Zeroizing;

const MAX_REMOTE_QUOTE_REPLY_BYTES: usize = 28 * 1024;

impl Microsoft365Transport {
    /// Sends one plaintext reply that quotes exactly one opaque message in an
    /// opaque Microsoft 365 chat. It accepts only Graph's documented `201`
    /// receipt; every other outcome is classified by the shared chat-send
    /// status mapping so the controller never retries an ambiguous POST.
    pub(super) fn reply_to_chat_message_with_quote(
        &mut self,
        chat: &ProviderResourceRef,
        messages: &[ProviderResourceRef],
        body: &str,
    ) -> Result<crate::integrations::CommandReceipt, TransportError> {
        let chat_id = chat.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
        })?;
        if messages.is_empty() || messages.len() > MAX_REMOTE_QUOTE_SOURCES {
            return Err(TransportError::InvalidCommand(
                "a quote reply needs one to ten quoted chat messages".into(),
            ));
        }
        let mut message_ids = Vec::with_capacity(messages.len());
        for message in messages {
            let id = message.microsoft365_id().ok_or_else(|| {
                TransportError::Unsupported(
                    "Microsoft Graph cannot target a local preview chat message".into(),
                )
            })?;
            reject_dot_segment(id)?;
            if message_ids.contains(&id) {
                return Err(TransportError::InvalidCommand(
                    "a quote reply cannot quote the same chat message twice".into(),
                ));
            }
            message_ids.push(id);
        }
        validate_quote_reply_body(body)?;

        let url = chat_quote_reply_url(self.session.cloud(), chat_id)?;
        let payload = ReplyWithQuote {
            message_ids,
            reply_message: QuoteReplyMessage {
                // Graph v1.0 takes `replyMessage.body` as an itemBody. Input
                // is restricted to plain text, so declare it explicitly; no
                // generic-send `importance`/HTML fields are added to this
                // distinct endpoint.
                body: QuoteReplyBody {
                    content_type: "text",
                    content: body,
                },
            },
        };
        let request_body = Zeroizing::new(serde_json::to_vec(&payload).map_err(|_| {
            TransportError::Unavailable("could not prepare the quote reply for delivery".into())
        })?);
        let response = self
            .session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(map_quote_reply_http_error)?;

        self.handle_chat_message_status(response, "microsoft365:chat-message-quote-reply")
    }
}

fn validate_quote_reply_body(body: &str) -> Result<(), TransportError> {
    if body.trim().is_empty() {
        return Err(TransportError::InvalidCommand(
            "quote reply body is required".into(),
        ));
    }
    if body.contains('\0') {
        return Err(TransportError::InvalidCommand(
            "quote reply body contains a null byte".into(),
        ));
    }
    if body.len() > MAX_REMOTE_QUOTE_REPLY_BYTES {
        return Err(TransportError::InvalidCommand(
            "quote reply body exceeds the remote delivery limit".into(),
        ));
    }
    Ok(())
}

fn chat_quote_reply_url(cloud: GraphCloud, chat_id: &str) -> Result<Url, TransportError> {
    // `url` can normalize dot segments. A provider identifier must not be
    // allowed to retarget this token-bearing route through `.` or `..`.
    reject_dot_segment(chat_id)?;
    let url = cloud
        .graph_route(&["chats", chat_id, "messages", "replyWithQuote"], None)
        .map_err(|_| {
            TransportError::Unavailable(
                "could not prepare the Microsoft Graph quote reply route".into(),
            )
        })?;
    Ok(url)
}

fn map_quote_reply_http_error(error: GraphHttpError) -> TransportError {
    map_mutation_http_error(
        error,
        "Microsoft Graph could not confirm quote reply delivery",
        "Microsoft Graph quote reply delivery is unavailable",
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplyWithQuote<'a> {
    message_ids: Vec<&'a str>,
    reply_message: QuoteReplyMessage<'a>,
}

#[derive(Serialize)]
struct QuoteReplyMessage<'a> {
    body: QuoteReplyBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QuoteReplyBody<'a> {
    content_type: &'static str,
    content: &'a str,
}

#[cfg(test)]
mod tests {
    use crate::integrations::cloud::GraphCloud;
    #[test]
    fn dot_segment_chat_ids_are_rejected_before_building_the_quote_route() {
        for id in [".", "..", " .. "] {
            assert!(matches!(
                super::chat_quote_reply_url(GraphCloud::Global, id),
                Err(crate::integrations::TransportError::Unsupported(_))
            ));
        }
        assert!(super::chat_quote_reply_url(GraphCloud::Global, "19:abc@thread.v2").is_ok());
    }

    use super::{GraphHttpError, MAX_REMOTE_QUOTE_REPLY_BYTES, Microsoft365Transport};
    use crate::integrations::microsoft365::{GraphHttpClient, GraphHttpStatus, GraphJsonRequest};
    use crate::integrations::{
        AccountIdentity, CommandDisposition, ConnectionStatus, ProviderResourceRef, TransportError,
        WorkspaceCommand, WorkspaceTransport,
    };
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use url::Url;

    struct RecordedPost {
        url: String,
        content_type: String,
        body: Vec<u8>,
    }

    struct RecordingPostState {
        responses: Mutex<VecDeque<Result<GraphHttpStatus, GraphHttpError>>>,
        posts: Mutex<Vec<RecordedPost>>,
    }

    struct RecordingPostClient {
        state: Arc<RecordingPostState>,
    }

    impl RecordingPostClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpStatus, GraphHttpError>>,
        ) -> (Self, Arc<RecordingPostState>) {
            let state = Arc::new(RecordingPostState {
                responses: Mutex::new(responses.into_iter().collect()),
                posts: Mutex::new(Vec::new()),
            });
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl GraphHttpClient for RecordingPostClient {
        fn get_me(
            &self,
            _url: &Url,
            _access_token: &str,
        ) -> Result<super::super::GraphHttpResponse, GraphHttpError> {
            Err(GraphHttpError::Unsupported)
        }

        fn post_json_status(
            &self,
            request: GraphJsonRequest<'_>,
            _access_token: &str,
        ) -> Result<GraphHttpStatus, GraphHttpError> {
            self.state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .push(RecordedPost {
                    url: request.url.as_str().into(),
                    content_type: request.content_type.into(),
                    body: request.body.into(),
                });
            self.state
                .responses
                .lock()
                .expect("configured response list must not be poisoned")
                .pop_front()
                .expect("a post response must be configured")
        }
    }

    fn quote_reply(chat: &str, message: &str, body: &str) -> WorkspaceCommand {
        WorkspaceCommand::ReplyToChatMessageWithQuote {
            chat: ProviderResourceRef::microsoft365(chat),
            messages: vec![ProviderResourceRef::microsoft365(message)],
            body: body.into(),
        }
    }

    fn quote_reply_many(ids: &[String]) -> WorkspaceCommand {
        WorkspaceCommand::ReplyToChatMessageWithQuote {
            chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
            messages: ids
                .iter()
                .map(|id| ProviderResourceRef::microsoft365(id.clone()))
                .collect(),
            body: "Many".into(),
        }
    }

    fn post_count(state: &RecordingPostState) -> usize {
        state
            .posts
            .lock()
            .expect("recorded post list must not be poisoned")
            .len()
    }

    #[test]
    fn sends_ten_quotes_with_exact_wire_and_no_importance() {
        let (client, state) = RecordingPostClient::new([Ok(GraphHttpStatus {
            status: 201,
            retry_after_seconds: None,
        })]);
        let mut transport = connected_transport(Box::new(client));
        let ids: Vec<String> = (0..10).map(|n| format!("m{n}")).collect();
        transport
            .dispatch(quote_reply_many(&ids))
            .expect("ten distinct sources must be accepted");
        let posts = state
            .posts
            .lock()
            .expect("recorded post list must not be poisoned");
        assert_eq!(
            std::str::from_utf8(&posts[0].body).expect("utf8 body"),
            r#"{"messageIds":["m0","m1","m2","m3","m4","m5","m6","m7","m8","m9"],"replyMessage":{"body":{"contentType":"text","content":"Many"}}}"#
        );
        assert!(!String::from_utf8_lossy(&posts[0].body).contains("importance"));
    }

    #[test]
    fn single_quote_wire_bytes_are_unchanged() {
        let (client, state) = RecordingPostClient::new([Ok(GraphHttpStatus {
            status: 201,
            retry_after_seconds: None,
        })]);
        let mut transport = connected_transport(Box::new(client));
        transport
            .dispatch(quote_reply("19:chat@thread.v2", "m1", "Hi"))
            .expect("one source must be accepted");
        let posts = state
            .posts
            .lock()
            .expect("recorded post list must not be poisoned");
        assert_eq!(
            std::str::from_utf8(&posts[0].body).expect("utf8 body"),
            r#"{"messageIds":["m1"],"replyMessage":{"body":{"contentType":"text","content":"Hi"}}}"#
        );
    }

    #[test]
    fn rejects_eleven_empty_and_duplicate_sources_before_posting() {
        let (client, state) = RecordingPostClient::new([]);
        let mut transport = connected_transport(Box::new(client));
        let eleven: Vec<String> = (0..11).map(|n| format!("m{n}")).collect();
        let duplicate = vec!["m1".to_string(), "m2".into(), "m1".into()];
        for ids in [eleven, duplicate, Vec::new()] {
            assert!(matches!(
                transport.dispatch(quote_reply_many(&ids)),
                Err(TransportError::InvalidCommand(_))
            ));
        }
        assert_eq!(post_count(&state), 0);
    }

    #[test]
    fn dot_segment_source_is_rejected_before_posting() {
        let (client, state) = RecordingPostClient::new([]);
        let mut transport = connected_transport(Box::new(client));
        let ids = vec!["m1".to_string(), "..".into()];
        assert!(transport.dispatch(quote_reply_many(&ids)).is_err());
        assert_eq!(post_count(&state), 0);
    }

    fn connected_transport(client: Box<dyn GraphHttpClient>) -> Microsoft365Transport {
        let mut transport = Microsoft365Transport::with_client("sensitive-access-token", client);
        let identity = AccountIdentity {
            account_id: "a1".into(),
            display_name: "Ada Lovelace".into(),
            email: Some("ada@example.test".into()),
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        transport
    }

    #[test]
    fn sends_one_content_only_quote_reply_to_the_encoded_graph_route() {
        let (client, state) = RecordingPostClient::new([Ok(GraphHttpStatus {
            status: 201,
            retry_after_seconds: None,
        })]);
        let mut transport = connected_transport(Box::new(client));

        let receipt = transport
            .dispatch(quote_reply(
                "19:chat id@thread.v2",
                "opaque message id",
                "A plain reply",
            ))
            .expect("only a 201 response must acknowledge a quote reply");

        assert_eq!(receipt.command_id, "microsoft365:chat-message-quote-reply");
        assert_eq!(receipt.disposition, CommandDisposition::AppliedRemotely);
        let posts = state
            .posts
            .lock()
            .expect("recorded post list must not be poisoned");
        assert_eq!(posts.len(), 1);
        let route = Url::parse(&posts[0].url).expect("the recorded route must be a URL");
        assert_eq!(route.scheme(), "https");
        assert_eq!(route.host_str(), Some("graph.microsoft.com"));
        assert_eq!(
            route
                .path_segments()
                .expect("Graph route must have segments")
                .collect::<Vec<_>>(),
            [
                "v1.0",
                "chats",
                "19:chat%20id@thread.v2",
                "messages",
                "replyWithQuote"
            ]
        );
        assert!(!posts[0].url.contains("chat id"));
        assert_eq!(posts[0].content_type, "application/json");
        let body: Value =
            serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
        assert_eq!(body["messageIds"], serde_json::json!(["opaque message id"]));
        assert_eq!(body["replyMessage"]["body"]["content"], "A plain reply");
        assert_eq!(body["replyMessage"]["body"]["contentType"], "text");
        assert!(body.get("importance").is_none());
        assert!(body["replyMessage"].get("importance").is_none());
    }

    #[test]
    fn rejects_local_or_mixed_targets_before_posting() {
        let (client, state) = RecordingPostClient::new([Ok(GraphHttpStatus {
            status: 201,
            retry_after_seconds: None,
        })]);
        let mut transport = connected_transport(Box::new(client));
        let local = WorkspaceCommand::ReplyToChatMessageWithQuote {
            chat: ProviderResourceRef::local_preview("fixture-chat"),
            messages: vec![ProviderResourceRef::local_preview("fixture-message")],
            body: "Must remain remote-only".into(),
        };
        let mixed = WorkspaceCommand::ReplyToChatMessageWithQuote {
            chat: ProviderResourceRef::microsoft365("19:remote@thread.v2"),
            messages: vec![ProviderResourceRef::local_preview("fixture-message")],
            body: "Must not cross providers".into(),
        };

        assert!(matches!(
            transport.dispatch(local),
            Err(TransportError::Unsupported(_))
        ));
        assert!(matches!(
            transport.dispatch(mixed),
            Err(TransportError::Unsupported(_))
        ));
        assert!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn accepts_only_201_and_keeps_rejections_and_ambiguity_nonleaking() {
        for (status, expected) in [(202, "could not confirm"), (400, "rejected")] {
            let (client, _state) = RecordingPostClient::new([Ok(GraphHttpStatus {
                status,
                retry_after_seconds: None,
            })]);
            let mut transport = connected_transport(Box::new(client));
            let error = transport
                .dispatch(quote_reply(
                    "19:private@thread.v2",
                    "private-message",
                    "secret",
                ))
                .expect_err("only 201 must be a quote-reply receipt");
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains("private"));
            assert!(!error.to_string().contains("secret"));
        }

        let (client, state) = RecordingPostClient::new([Err(GraphHttpError::Network)]);
        let mut transport = connected_transport(Box::new(client));
        let error = transport
            .dispatch(quote_reply("19:chat@thread.v2", "message", "confidential"))
            .expect_err("network result is delivery-uncertain");
        assert!(matches!(error, TransportError::Unavailable(_)));
        let oversized = "x".repeat(MAX_REMOTE_QUOTE_REPLY_BYTES + 1);
        assert!(matches!(
            transport.dispatch(quote_reply("19:chat@thread.v2", "message", &oversized)),
            Err(TransportError::InvalidCommand(_))
        ));
        assert_eq!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .len(),
            1,
            "oversized quote reply must not be posted"
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url = super::chat_quote_reply_url(cloud, "c")
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
