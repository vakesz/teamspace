//! Microsoft Graph channel root-post and reply reaction delivery.
//!
//! Setting and unsetting a reaction use distinct Graph action routes, but both
//! carry their full explicit selection targets. A `204 No Content` is the
//! only documented success response. The adapter returns no optimistic
//! projection: the app-owned transport lifecycle decides when to refresh the
//! exact selected feed after a confirmed or uncertain outcome.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segments};
use super::reactions::reaction_request_body;
use super::{GraphHttpError, GraphHttpStatus, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::{CommandReceipt, ProviderResourceRef, TransportError};
use crate::shared::reaction::ReactionKind;
use url::Url;

impl Microsoft365Transport {
    /// Sets or unsets one reaction on one opaque Microsoft 365 channel root
    /// post. The provider route cannot be inferred from a message ID alone,
    /// so all three validated parent targets remain explicit here.
    pub(super) fn set_channel_message_reaction(
        &mut self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Result<CommandReceipt, TransportError> {
        let team_id = team.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
        })?;
        let channel_id = channel.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel".into(),
            )
        })?;
        let post_id = post.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel root post".into(),
            )
        })?;
        let url = channel_message_reaction_url(
            self.session.cloud(),
            team_id,
            channel_id,
            post_id,
            selected,
        )?;
        self.deliver_channel_message_reaction(
            url,
            reaction,
            "microsoft365:channel-message-reaction",
        )
    }

    /// Sets or unsets one app-supported reaction on one opaque reply beneath
    /// one selected Microsoft 365 channel root post. A reply ID alone is
    /// never enough to infer a Graph route, so all four parent and reply
    /// targets stay explicit until the URL is constructed.
    pub(super) fn set_channel_message_reply_reaction(
        &mut self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reply: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Result<CommandReceipt, TransportError> {
        let team_id = team.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
        })?;
        let channel_id = channel.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel".into(),
            )
        })?;
        let post_id = post.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel root post".into(),
            )
        })?;
        let reply_id = reply.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported(
                "Microsoft Graph cannot target a local preview channel reply".into(),
            )
        })?;
        let url = channel_message_reply_reaction_url(
            self.session.cloud(),
            team_id,
            channel_id,
            post_id,
            reply_id,
            selected,
        )?;
        self.deliver_channel_message_reaction(
            url,
            reaction,
            "microsoft365:channel-message-reply-reaction",
        )
    }

    fn deliver_channel_message_reaction(
        &mut self,
        url: Url,
        reaction: ReactionKind,
        command_id: &'static str,
    ) -> Result<CommandReceipt, TransportError> {
        let request_body = reaction_request_body(reaction).ok_or_else(|| {
            TransportError::Unavailable(
                "could not prepare the channel reaction for delivery".into(),
            )
        })?;
        let response = self
            .session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(map_channel_reaction_http_error)?;

        self.handle_channel_message_reaction_status(response, command_id)
    }

    fn handle_channel_message_reaction_status(
        &mut self,
        response: GraphHttpStatus,
        command_id: &'static str,
    ) -> Result<CommandReceipt, TransportError> {
        // Graph's setReaction and unsetReaction actions both document
        // `204 No Content` as their sole successful response.
        self.classify_mutation_status(
            response,
            204,
            command_id,
            MutationMessages {
                rejected: "Microsoft Graph rejected the channel reaction",
                unconfirmed: "Microsoft Graph could not confirm channel reaction delivery",
            },
        )
    }
}

fn channel_message_reaction_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
    post_id: &str,
    selected: bool,
) -> Result<Url, TransportError> {
    // `url` silently drops "." and ".." path segments, which would retarget
    // the request. Provider IDs are never dot segments.
    reject_dot_segments(&[team_id, channel_id, post_id])?;
    let action = if selected {
        "setReaction"
    } else {
        "unsetReaction"
    };
    let url = cloud
        .graph_route(
            &[
                "teams", team_id, "channels", channel_id, "messages", post_id, action,
            ],
            None,
        )
        .map_err(|_| {
            TransportError::Unavailable(
                "could not prepare the Microsoft Graph channel reaction route".into(),
            )
        })?;
    Ok(url)
}

fn channel_message_reply_reaction_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
    post_id: &str,
    reply_id: &str,
    selected: bool,
) -> Result<Url, TransportError> {
    // `url` silently drops "." and ".." path segments, which would retarget
    // the request. Provider IDs are never dot segments.
    reject_dot_segments(&[team_id, channel_id, post_id, reply_id])?;
    let action = if selected {
        "setReaction"
    } else {
        "unsetReaction"
    };
    let url = cloud
        .graph_route(
            &[
                "teams", team_id, "channels", channel_id, "messages", post_id, "replies", reply_id,
                action,
            ],
            None,
        )
        .map_err(|_| {
            TransportError::Unavailable(
                "could not prepare the Microsoft Graph channel reply reaction route".into(),
            )
        })?;
    Ok(url)
}

fn map_channel_reaction_http_error(error: GraphHttpError) -> TransportError {
    map_mutation_http_error(
        error,
        "Microsoft Graph could not confirm channel reaction delivery",
        "Microsoft Graph channel reaction delivery is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use crate::integrations::cloud::GraphCloud;
    #[test]
    fn dot_segment_provider_ids_are_rejected_before_building_a_route() {
        for ids in [("..", "c", "m"), ("t", ".", "m"), ("t", "c", "..")] {
            assert!(matches!(
                super::channel_message_reaction_url(GraphCloud::Global, ids.0, ids.1, ids.2, true),
                Err(TransportError::Unsupported(_))
            ));
        }
        assert!(
            super::channel_message_reaction_url(GraphCloud::Global, "t", "c", "m", true).is_ok()
        );

        for ids in [
            ("..", "c", "m", "r"),
            ("t", ".", "m", "r"),
            ("t", "c", "..", "r"),
            ("t", "c", "m", "."),
        ] {
            assert!(matches!(
                super::channel_message_reply_reaction_url(
                    GraphCloud::Global,
                    ids.0,
                    ids.1,
                    ids.2,
                    ids.3,
                    true
                ),
                Err(TransportError::Unsupported(_))
            ));
        }
        assert!(
            super::channel_message_reply_reaction_url(GraphCloud::Global, "t", "c", "m", "r", true)
                .is_ok()
        );
    }

    use super::{GraphHttpError, GraphHttpStatus, Microsoft365Transport};
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
        team: &str,
        channel: &str,
        post: &str,
        reaction: ReactionKind,
        selected: bool,
    ) -> WorkspaceCommand {
        WorkspaceCommand::SetChannelMessageReaction {
            team: ProviderResourceRef::microsoft365(team),
            channel: ProviderResourceRef::microsoft365(channel),
            post: ProviderResourceRef::microsoft365(post),
            reaction,
            selected,
        }
    }

    fn remote_reply_reaction(
        team: &str,
        channel: &str,
        post: &str,
        reply: &str,
        reaction: ReactionKind,
        selected: bool,
    ) -> WorkspaceCommand {
        WorkspaceCommand::SetChannelMessageReplyReaction {
            team: ProviderResourceRef::microsoft365(team),
            channel: ProviderResourceRef::microsoft365(channel),
            post: ProviderResourceRef::microsoft365(post),
            reply: ProviderResourceRef::microsoft365(reply),
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
    fn set_and_unset_reactions_use_encoded_routes_and_a_status_only_payload() {
        for (selected, action) in [(true, "setReaction"), (false, "unsetReaction")] {
            let (client, state) = RecordingPostClient::new([status(204, None)]);
            let mut transport = connected_transport(Box::new(client));

            let receipt = transport
                .dispatch(remote_reaction(
                    "team id/with?characters",
                    "channel id/with?characters",
                    "message id/with?characters",
                    ReactionKind::Like,
                    selected,
                ))
                .expect("204 must acknowledge the channel reaction");

            assert_eq!(receipt.command_id, "microsoft365:channel-message-reaction");
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
                    "messages",
                    "message%20id%2Fwith%3Fcharacters",
                    action,
                ]
            );
            assert!(!posts[0].url.contains("team id/with?characters"));
            assert!(!posts[0].url.contains("channel id/with?characters"));
            assert!(!posts[0].url.contains("message id/with?characters"));
            assert_eq!(posts[0].content_type, "application/json");
            let body: Value =
                serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
            assert_eq!(body["reactionType"], "👍");
        }
    }

    #[test]
    fn set_and_unset_reply_reactions_use_only_the_exact_encoded_reply_route() {
        for (selected, action) in [(true, "setReaction"), (false, "unsetReaction")] {
            let (client, state) = RecordingPostClient::new([status(204, None)]);
            let mut transport = connected_transport(Box::new(client));

            let receipt = transport
                .dispatch(remote_reply_reaction(
                    "team id/with?characters",
                    "channel id/with?characters",
                    "post id/with?characters",
                    "reply id/with?characters",
                    ReactionKind::Heart,
                    selected,
                ))
                .expect("204 must acknowledge the channel reply reaction");

            assert_eq!(
                receipt.command_id,
                "microsoft365:channel-message-reply-reaction"
            );
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
                    "messages",
                    "post%20id%2Fwith%3Fcharacters",
                    "replies",
                    "reply%20id%2Fwith%3Fcharacters",
                    action,
                ]
            );
            assert!(!posts[0].url.contains("team id/with?characters"));
            assert!(!posts[0].url.contains("channel id/with?characters"));
            assert!(!posts[0].url.contains("post id/with?characters"));
            assert!(!posts[0].url.contains("reply id/with?characters"));
            assert_eq!(posts[0].content_type, "application/json");
            let body: Value =
                serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
            assert_eq!(body, serde_json::json!({ "reactionType": "❤" }));
        }
    }

    #[test]
    fn local_or_mixed_targets_are_rejected_before_posting() {
        let (client, state) = RecordingPostClient::new([status(204, None)]);
        let mut transport = connected_transport(Box::new(client));

        for (team, channel, post) in [
            (
                ProviderResourceRef::local_preview("local-team"),
                ProviderResourceRef::local_preview("local-channel"),
                ProviderResourceRef::local_preview("local-message"),
            ),
            (
                ProviderResourceRef::microsoft365("remote-team"),
                ProviderResourceRef::local_preview("local-channel"),
                ProviderResourceRef::microsoft365("remote-message"),
            ),
            (
                ProviderResourceRef::microsoft365("remote-team"),
                ProviderResourceRef::microsoft365("remote-channel"),
                ProviderResourceRef::local_preview("local-message"),
            ),
        ] {
            let error = transport
                .dispatch(WorkspaceCommand::SetChannelMessageReaction {
                    team,
                    channel,
                    post,
                    reaction: ReactionKind::Like,
                    selected: true,
                })
                .expect_err("non-Microsoft-365 targets must not reach Graph");
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
    fn reply_reactions_reject_non_microsoft_targets_before_posting() {
        let (client, state) = RecordingPostClient::new([status(204, None)]);
        let mut transport = connected_transport(Box::new(client));

        for command in [
            WorkspaceCommand::SetChannelMessageReplyReaction {
                team: ProviderResourceRef::local_preview("local-team"),
                channel: ProviderResourceRef::local_preview("local-channel"),
                post: ProviderResourceRef::local_preview("local-post"),
                reply: ProviderResourceRef::local_preview("local-reply"),
                reaction: ReactionKind::Like,
                selected: true,
            },
            WorkspaceCommand::SetChannelMessageReplyReaction {
                team: ProviderResourceRef::microsoft365("remote-team"),
                channel: ProviderResourceRef::microsoft365("remote-channel"),
                post: ProviderResourceRef::microsoft365("remote-post"),
                reply: ProviderResourceRef::local_preview("local-reply"),
                reaction: ReactionKind::Like,
                selected: true,
            },
        ] {
            let error = transport
                .dispatch(command)
                .expect_err("invalid reply reactions must not reach Graph");
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
    fn supported_channel_reactions_emit_only_canonical_unicode() {
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
                .dispatch(remote_reaction("team", "channel", "message", kind, true))
                .expect("typed root-post reaction must be delivered");
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
    fn reaction_statuses_preserve_or_invalidate_connection_without_leaking_inputs() {
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
                    "sensitive-team",
                    "sensitive-channel",
                    "sensitive-message",
                    ReactionKind::Like,
                    true,
                ))
                .expect_err("only 204 can acknowledge a channel reaction");

            assert!(error.to_string().contains(error_fragment));
            assert!(!error.to_string().contains("sensitive-team"));
            assert!(!error.to_string().contains("sensitive-channel"));
            assert!(!error.to_string().contains("sensitive-message"));
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
    fn reply_reaction_requires_204_and_redacts_all_route_targets() {
        let (client, state) = RecordingPostClient::new([status(200, None)]);
        let mut transport = connected_transport(Box::new(client));

        let error = transport
            .dispatch(remote_reply_reaction(
                "sensitive-team",
                "sensitive-channel",
                "sensitive-post",
                "sensitive-reply",
                ReactionKind::Like,
                true,
            ))
            .expect_err("a non-204 reply reaction result must not be acknowledged");

        assert!(matches!(error, TransportError::Unavailable(_)));
        let rendered = error.to_string();
        for sensitive in [
            "sensitive-team",
            "sensitive-channel",
            "sensitive-post",
            "sensitive-reply",
            "sensitive-access-token",
        ] {
            assert!(!rendered.contains(sensitive));
        }
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
    fn ambiguous_network_failures_never_acknowledge_delivery() {
        let (client, state) = RecordingPostClient::new([Err(GraphHttpError::Network)]);
        let mut transport = connected_transport(Box::new(client));
        let error = transport
            .dispatch(remote_reaction(
                "team",
                "channel",
                "message",
                ReactionKind::Angry,
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
            for route in [
                super::channel_message_reaction_url(cloud, "t", "c", "m", true),
                super::channel_message_reply_reaction_url(cloud, "t", "c", "m", "r", true),
            ] {
                let url = route.unwrap_or_else(|_| panic!("route must build"));
                assert_eq!(url.scheme(), "https");
                assert_eq!(url.host_str(), Some(cloud.graph_host()));
                assert!(url.path().starts_with("/v1.0/"));
            }
        }
    }
}
