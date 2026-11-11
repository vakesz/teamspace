//! Microsoft Graph root-channel-post delivery.
//!
//! A normal channel `POST` is non-idempotent. This adapter treats only its
//! documented `201 Created` response as delivery confirmation and deliberately
//! never reads a success body: a local parse failure after Graph accepted the
//! post must not become a reason to send duplicate content. Network and server
//! outcomes remain ambiguous for the app-owned delivery lifecycle to handle.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segments};
use super::{GraphHttpError, GraphHttpStatus, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use serde::Serialize;
use url::Url;
use zeroize::Zeroizing;

/// Keep ordinary text posts below the documented approximate Teams channel
/// message cap while leaving room for provider serialization overhead.
const MAX_REMOTE_CHANNEL_POST_BYTES: usize = 64 * 1024;

impl Microsoft365Transport {
    /// Sends one text-only root post to an opaque Microsoft 365 channel.
    ///
    /// The transport dispatch validates provider ownership before this method
    /// is reached. Rechecking the opaque IDs and payload here preserves that
    /// boundary for future call paths and prevents local preview IDs or an
    /// invalid body from ever becoming a token-bearing Graph route.
    pub(super) fn send_channel_post(
        &mut self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        body: &str,
    ) -> Result<CommandReceipt, TransportError> {
        let team_id = team.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
        })?;
        let channel_id = channel.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel".into(),
            )
        })?;
        validate_channel_post_body(body)?;

        let url = channel_messages_url(self.session.cloud(), team_id, channel_id)?;
        let payload = CreateChannelPost {
            body: TextBody {
                content_type: "text",
                content: body,
            },
        };
        // Serialize into a short-lived zeroizing buffer. The HTTP client
        // consumes only a request-scoped copy and never logs user content.
        let request_body = Zeroizing::new(serde_json::to_vec(&payload).map_err(|_| {
            TransportError::Unavailable("could not prepare the channel post for delivery".into())
        })?);
        let response = self
            .session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(map_channel_post_http_error)?;

        self.handle_channel_post_status(response)
    }

    fn handle_channel_post_status(
        &mut self,
        response: GraphHttpStatus,
    ) -> Result<CommandReceipt, TransportError> {
        // Normal creation is synchronous. The distinct migration/import
        // response shape is deliberately not accepted as a delivery receipt.
        self.classify_mutation_status(
            response,
            201,
            "microsoft365:channel-post",
            MutationMessages {
                rejected: "Microsoft Graph rejected the channel post",
                unconfirmed: "Microsoft Graph could not confirm channel post delivery",
            },
        )
    }
}

fn validate_channel_post_body(body: &str) -> Result<(), TransportError> {
    if body.trim().is_empty() {
        return Err(TransportError::InvalidCommand(
            "channel post body is required".into(),
        ));
    }
    if body.contains('\0') {
        return Err(TransportError::InvalidCommand(
            "channel post body contains a null byte".into(),
        ));
    }
    if body.len() > MAX_REMOTE_CHANNEL_POST_BYTES {
        return Err(TransportError::InvalidCommand(
            "channel post body exceeds the remote delivery limit".into(),
        ));
    }
    Ok(())
}

fn channel_messages_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
) -> Result<Url, TransportError> {
    // `url` silently drops "." and ".." path segments, which would retarget
    // the request. Provider IDs are never dot segments.
    reject_dot_segments(&[team_id, channel_id])?;
    let url = cloud
        .graph_route(
            &["teams", team_id, "channels", channel_id, "messages"],
            None,
        )
        .map_err(|_| {
            TransportError::Unavailable(
                "could not prepare the Microsoft Graph channel route".into(),
            )
        })?;
    Ok(url)
}

fn map_channel_post_http_error(error: GraphHttpError) -> TransportError {
    map_mutation_http_error(
        error,
        "Microsoft Graph could not confirm channel post delivery",
        "Microsoft Graph channel posting is unavailable",
    )
}

#[derive(Serialize)]
struct CreateChannelPost<'a> {
    body: TextBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TextBody<'a> {
    content_type: &'static str,
    content: &'a str,
}

#[cfg(test)]
mod tests {
    use super::{
        GraphHttpError, GraphHttpStatus, MAX_REMOTE_CHANNEL_POST_BYTES, Microsoft365Transport,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphJsonRequest};
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

    fn remote_post(team: &str, channel: &str, body: &str) -> WorkspaceCommand {
        WorkspaceCommand::SendChannelPost {
            team: ProviderResourceRef::microsoft365(team),
            channel: ProviderResourceRef::microsoft365(channel),
            body: body.into(),
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
    fn sends_text_to_the_encoded_root_channel_message_route_without_reading_a_success_body() {
        let (client, state) = RecordingPostClient::new([status(201, None)]);
        let mut transport = connected_transport(Box::new(client));

        let receipt = transport
            .dispatch(remote_post(
                "team id/with?characters",
                "channel id/with?characters",
                "Hello \"channel\"",
            ))
            .expect("201 must acknowledge the root channel post");

        assert_eq!(receipt.command_id, "microsoft365:channel-post");
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
                "teams",
                "team%20id%2Fwith%3Fcharacters",
                "channels",
                "channel%20id%2Fwith%3Fcharacters",
                "messages"
            ]
        );
        assert!(!posts[0].url.contains("team id/with?characters"));
        assert!(!posts[0].url.contains("channel id/with?characters"));
        assert_eq!(posts[0].content_type, "application/json");
        let body: Value =
            serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
        assert_eq!(body["body"]["contentType"], "text");
        assert_eq!(body["body"]["content"], "Hello \"channel\"");
    }

    #[test]
    fn dot_segment_provider_ids_are_rejected_before_building_a_route() {
        for (team, channel) in [("..", "channel"), ("team", "."), (" .. ", "channel")] {
            assert!(matches!(
                super::channel_messages_url(GraphCloud::Global, team, channel),
                Err(TransportError::Unsupported(_))
            ));
        }
        assert!(
            super::channel_messages_url(GraphCloud::Global, "team", "19:abc@thread.tacv2").is_ok()
        );
    }

    #[test]
    fn local_or_mixed_targets_are_rejected_before_posting() {
        let (client, state) = RecordingPostClient::new([status(201, None)]);
        let mut transport = connected_transport(Box::new(client));

        for (team, channel) in [
            (
                ProviderResourceRef::local_preview("local-team"),
                ProviderResourceRef::local_preview("local-channel"),
            ),
            (
                ProviderResourceRef::microsoft365("remote-team"),
                ProviderResourceRef::local_preview("local-channel"),
            ),
            (
                ProviderResourceRef::local_preview("local-team"),
                ProviderResourceRef::microsoft365("remote-channel"),
            ),
        ] {
            let error = transport
                .dispatch(WorkspaceCommand::SendChannelPost {
                    team,
                    channel,
                    body: "This must remain local".into(),
                })
                .expect_err("non-Microsoft-365 channel targets must not post to Graph");
            assert!(matches!(error, TransportError::Unsupported(_)));
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
    fn payload_validation_rejects_empty_null_and_oversized_text_before_networking() {
        let (client, state) = RecordingPostClient::new([status(201, None)]);
        let mut transport = connected_transport(Box::new(client));

        for body in ["   ", "contains\0null"] {
            let error = transport
                .dispatch(remote_post("team", "channel", body))
                .expect_err("invalid body must not be posted");
            assert!(matches!(error, TransportError::InvalidCommand(_)));
        }
        let oversized = "x".repeat(MAX_REMOTE_CHANNEL_POST_BYTES + 1);
        let error = transport
            .dispatch(remote_post("team", "channel", &oversized))
            .expect_err("oversized body must not be posted");
        assert!(matches!(error, TransportError::InvalidCommand(_)));
        assert!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .is_empty()
        );
    }

    #[test]
    fn post_statuses_preserve_or_invalidate_connection_state_without_leaking_sensitive_data() {
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
                .dispatch(remote_post(
                    "sensitive-team",
                    "sensitive-channel",
                    "Sensitive channel post",
                ))
                .expect_err("only 201 can acknowledge normal root post delivery");

            assert!(error.to_string().contains(error_fragment));
            assert!(!error.to_string().contains("sensitive-team"));
            assert!(!error.to_string().contains("sensitive-channel"));
            assert!(!error.to_string().contains("Sensitive channel post"));
            assert!(!error.to_string().contains("sensitive-access-token"));
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
    fn ambiguous_network_failures_never_leak_body_or_acknowledge_delivery() {
        let (client, state) = RecordingPostClient::new([Err(GraphHttpError::Network)]);
        let mut transport = connected_transport(Box::new(client));
        let error = transport
            .dispatch(remote_post("team", "channel", "confidential channel body"))
            .expect_err("network delivery outcome is ambiguous");
        assert!(matches!(error, TransportError::Unavailable(_)));
        assert!(!error.to_string().contains("confidential channel body"));
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
            let url = super::channel_messages_url(cloud, "t", "c")
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
