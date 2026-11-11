//! Microsoft Graph selected-chat message reaction delivery.
//!
//! This module owns only the exact `/chats/{chat}/messages/{message}`
//! set/unset routes. It deliberately does not share a generic message-action
//! helper with channel reactions: the complete enclosing target is part of the
//! safety contract, and a chat message ID must never be reinterpreted as a
//! channel-message route.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segments};
use super::reactions::reaction_request_body;
use super::{GraphHttpError, GraphHttpStatus, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use crate::shared::reaction::ReactionKind;
use url::Url;

impl Microsoft365Transport {
    /// Sets or unsets one app-supported reaction on one selected Microsoft
    /// 365 chat message. A bare message ID cannot determine its enclosing
    /// chat, so both opaque targets stay explicit through route construction.
    pub(super) fn set_chat_message_reaction(
        &mut self,
        chat: &ProviderResourceRef,
        message: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Result<CommandReceipt, TransportError> {
        let chat_id = chat.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
        })?;
        let message_id = message.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview chat message".into(),
            )
        })?;
        let url = chat_message_reaction_url(self.session.cloud(), chat_id, message_id, selected)?;
        let request_body = reaction_request_body(reaction).ok_or_else(|| {
            TransportError::Unavailable("could not prepare the chat reaction for delivery".into())
        })?;
        let response = self
            .session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(map_chat_reaction_http_error)?;

        self.handle_chat_message_reaction_status(response)
    }

    fn handle_chat_message_reaction_status(
        &mut self,
        response: GraphHttpStatus,
    ) -> Result<CommandReceipt, TransportError> {
        // Graph documents only `204 No Content` for both setReaction and
        // unsetReaction. A body or any other 2xx is not proof.
        self.classify_mutation_status(
            response,
            204,
            "microsoft365:chat-message-reaction",
            MutationMessages {
                rejected: "Microsoft Graph rejected the chat reaction",
                unconfirmed: "Microsoft Graph could not confirm chat reaction delivery",
            },
        )
    }
}

fn chat_message_reaction_url(
    cloud: GraphCloud,
    chat_id: &str,
    message_id: &str,
    selected: bool,
) -> Result<Url, TransportError> {
    // `url` silently normalizes dot segments. Provider IDs must never get an
    // opportunity to retarget a route through `.` or `..`.
    reject_dot_segments(&[chat_id, message_id])?;
    let action = if selected {
        "setReaction"
    } else {
        "unsetReaction"
    };
    let url = cloud
        .graph_route(&["chats", chat_id, "messages", message_id, action], None)
        .map_err(|_| {
            TransportError::Unavailable(
                "could not prepare the Microsoft Graph chat reaction route".into(),
            )
        })?;
    Ok(url)
}

fn map_chat_reaction_http_error(error: GraphHttpError) -> TransportError {
    map_mutation_http_error(
        error,
        "Microsoft Graph could not confirm chat reaction delivery",
        "Microsoft Graph chat reaction delivery is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        GraphHttpError, GraphHttpStatus, Microsoft365Transport, chat_message_reaction_url,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphJsonRequest};
    use crate::integrations::{
        AccountIdentity, CommandDisposition, ConnectionStatus, ProviderResourceRef, TransportError,
        WorkspaceCommand, WorkspaceTransport,
    };
    use crate::shared::reaction::ReactionKind;
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
                .expect("a response must be configured")
        }
    }

    fn status(
        status: u16,
        retry_after_seconds: Option<u64>,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status,
            retry_after_seconds,
        })
    }

    fn remote_reaction(
        chat: &str,
        message: &str,
        reaction: ReactionKind,
        selected: bool,
    ) -> WorkspaceCommand {
        WorkspaceCommand::SetChatMessageReaction {
            chat: ProviderResourceRef::microsoft365(chat),
            message: ProviderResourceRef::microsoft365(message),
            reaction,
            selected,
        }
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
    fn set_and_unset_chat_reactions_use_only_the_exact_encoded_route() {
        for (selected, action) in [(true, "setReaction"), (false, "unsetReaction")] {
            let (client, state) = RecordingPostClient::new([status(204, None)]);
            let mut transport = connected_transport(Box::new(client));

            let receipt = transport
                .dispatch(remote_reaction(
                    "chat id/with?characters",
                    "message id/with?characters",
                    ReactionKind::Like,
                    selected,
                ))
                .expect("204 must acknowledge the chat reaction");

            assert_eq!(receipt.command_id, "microsoft365:chat-message-reaction");
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
                    "chat%20id%2Fwith%3Fcharacters",
                    "messages",
                    "message%20id%2Fwith%3Fcharacters",
                    action,
                ]
            );
            assert!(!posts[0].url.contains("chat id/with?characters"));
            assert!(!posts[0].url.contains("message id/with?characters"));
            assert_eq!(posts[0].content_type, "application/json");
            let body: Value =
                serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
            assert_eq!(body, serde_json::json!({ "reactionType": "👍" }));
        }
    }

    #[test]
    fn chat_reactions_reject_non_microsoft_targets_before_posting() {
        let (client, state) = RecordingPostClient::new([status(204, None)]);
        let mut transport = connected_transport(Box::new(client));

        for command in [
            WorkspaceCommand::SetChatMessageReaction {
                chat: ProviderResourceRef::local_preview("local-chat"),
                message: ProviderResourceRef::local_preview("local-message"),
                reaction: ReactionKind::Like,
                selected: true,
            },
            WorkspaceCommand::SetChatMessageReaction {
                chat: ProviderResourceRef::microsoft365("remote-chat"),
                message: ProviderResourceRef::local_preview("local-message"),
                reaction: ReactionKind::Like,
                selected: true,
            },
        ] {
            let error = transport
                .dispatch(command)
                .expect_err("invalid chat reactions must not reach Graph");
            assert!(matches!(
                error,
                TransportError::Unsupported(_) | TransportError::InvalidCommand(_)
            ));
        }
        assert!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn supported_chat_reactions_emit_only_canonical_unicode() {
        for (kind, expected) in [
            (ReactionKind::Like, "👍"),
            (ReactionKind::Heart, "❤"),
            (ReactionKind::Laugh, "😄"),
            (ReactionKind::Surprised, "😮"),
            (ReactionKind::Sad, "😢"),
            (ReactionKind::Angry, "😡"),
        ] {
            let (client, state) = RecordingPostClient::new([status(204, None)]);
            let mut transport = connected_transport(Box::new(client));
            transport
                .dispatch(remote_reaction("chat", "message", kind, true))
                .expect("typed reaction must be delivered");
            let posts = state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned");
            let body: Value =
                serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
            assert_eq!(body, serde_json::json!({ "reactionType": expected }));
        }
    }

    #[test]
    fn dot_segment_provider_ids_are_rejected_before_building_a_route() {
        for ids in [(".", "message"), ("chat", ".."), (" .. ", "message")] {
            assert!(matches!(
                chat_message_reaction_url(GraphCloud::Global, ids.0, ids.1, true),
                Err(TransportError::Unsupported(_))
            ));
        }
        assert!(chat_message_reaction_url(GraphCloud::Global, "chat", "message", true).is_ok());
    }

    #[test]
    fn statuses_require_204_and_redact_chat_targets_and_reaction() {
        let cases = [
            (200, None, false, "could not confirm"),
            (401, None, true, "authentication must be renewed"),
            (403, None, false, "authorization"),
            (429, Some(17), false, "throttled"),
            (400, None, false, "rejected"),
            (404, None, false, "rejected"),
            (409, None, false, "rejected"),
            (413, None, false, "rejected"),
            (422, None, false, "rejected"),
            (500, None, false, "could not confirm"),
        ];

        for (status_code, retry_after_seconds, identity_cleared, error_fragment) in cases {
            let (client, _state) =
                RecordingPostClient::new([status(status_code, retry_after_seconds)]);
            let mut transport = connected_transport(Box::new(client));

            let error = transport
                .dispatch(remote_reaction(
                    "sensitive-chat",
                    "sensitive-message",
                    ReactionKind::Like,
                    true,
                ))
                .expect_err("only 204 can acknowledge a chat reaction");

            assert!(error.to_string().contains(error_fragment));
            for sensitive in [
                "sensitive-chat",
                "sensitive-message",
                "sensitive-access-token",
            ] {
                assert!(!error.to_string().contains(sensitive));
            }
            if status_code == 429 {
                assert_eq!(
                    error,
                    TransportError::Throttled {
                        retry_after_seconds: Some(17)
                    }
                );
            }
            if identity_cleared {
                assert!(transport.identity.is_none());
                assert!(matches!(
                    transport.connection_status(),
                    ConnectionStatus::ReauthenticationRequired
                ));
            } else {
                assert!(transport.identity.is_some());
                assert!(matches!(
                    transport.connection_status(),
                    ConnectionStatus::Connected(identity) if identity.account_id == "a1"
                ));
            }
        }
    }

    #[test]
    fn ambiguous_network_failures_never_acknowledge_delivery() {
        let (client, state) = RecordingPostClient::new([Err(GraphHttpError::Network)]);
        let mut transport = connected_transport(Box::new(client));
        let error = transport
            .dispatch(remote_reaction(
                "chat",
                "message",
                ReactionKind::Heart,
                true,
            ))
            .expect_err("network delivery outcome is ambiguous");
        assert!(matches!(error, TransportError::Unavailable(_)));
        assert_eq!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url = super::chat_message_reaction_url(cloud, "c", "m", true)
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
