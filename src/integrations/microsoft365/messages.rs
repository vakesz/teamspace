//! Microsoft Graph chat-message delivery.
//!
//! This module owns the one non-idempotent chat mutation currently exposed by
//! the adapter. It deliberately returns a status-only receipt: a `201` means
//! Graph accepted the message, while an ambiguous network/server outcome is
//! left for the controller to reconcile with a fresh chat projection rather
//! than replaying a POST automatically.

use super::mutation::{MutationMessages, map_mutation_http_error, reject_dot_segment};
use super::rich_html::{MentionScope, wire_body_with_attachment};
use super::{GraphHttpError, GraphHttpStatus, GraphJsonRequest, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::{
    CommandReceipt, ComposedBody, ProviderResourceRef, RemoteChatMessageImportance, TransportError,
    UploadedFileHandle,
};
use serde::Serialize;
use url::Url;
use zeroize::Zeroizing;

impl Microsoft365Transport {
    /// Sends one plain-text message to an opaque Microsoft 365 chat target.
    ///
    /// The caller has already applied generic/provider ownership validation,
    /// but this method keeps its own target and payload checks so a future
    /// dispatch path cannot accidentally turn a local preview key into a
    /// Graph route.
    pub(super) fn send_chat_message(
        &mut self,
        chat: &ProviderResourceRef,
        body: &ComposedBody,
        importance: RemoteChatMessageImportance,
        attachment: Option<&UploadedFileHandle>,
    ) -> Result<CommandReceipt, TransportError> {
        let chat_id = chat.microsoft365_id().ok_or_else(|| {
            TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
        })?;
        // The adapter-side size bound lives with the renderer, independent
        // of the larger local draft limit.
        // A file attaches only if this session uploaded it: the adapter's
        // registry, not the caller, holds its reference and URL.
        let registered = match attachment {
            Some(handle) => Some(self.uploads.resolve(handle).ok_or_else(|| {
                TransportError::InvalidCommand(
                    "the uploaded file is no longer available; attach it again".into(),
                )
            })?),
            None => None,
        };
        let wire = wire_body_with_attachment(
            body,
            "message body",
            &MentionScope::Chat {
                directory: &self.member_ids,
                chat_id,
            },
            registered.map(|(reference, _, _)| reference),
        )?;

        let url = chat_messages_url(self.session.cloud(), chat_id)?;
        let payload = CreateChatMessage {
            body: WireBodyPayload {
                content_type: wire.content_type,
                content: &wire.content,
            },
            importance: importance.graph_value(),
            attachments: registered
                .map(|(reference, content_url, name)| AttachmentPayload {
                    id: reference,
                    content_type: "reference",
                    content_url,
                    name,
                })
                .into_iter()
                .collect(),
            mentions: wire
                .mentions
                .iter()
                .map(|mention| MentionPayload {
                    id: mention.id,
                    mention_text: &mention.display,
                    mentioned: MentionedPayload {
                        user: MentionedUser {
                            display_name: &mention.display,
                            id: &mention.user_id,
                            user_identity_type: "aadUser",
                        },
                    },
                })
                .collect(),
        };
        // The request serialization is kept in a short-lived, zeroizing
        // buffer. The HTTP implementation consumes a copy only for the
        // duration of this blocking request and never logs it.
        let request_body = Zeroizing::new(serde_json::to_vec(&payload).map_err(|_| {
            TransportError::Unavailable("could not prepare the chat message for delivery".into())
        })?);
        let response = self
            .session
            .post_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: request_body.as_slice(),
            })
            .map_err(map_message_http_error)?;

        self.handle_chat_message_status(response, "microsoft365:chat-message")
    }

    pub(super) fn handle_chat_message_status(
        &mut self,
        response: GraphHttpStatus,
        command_id: &'static str,
    ) -> Result<CommandReceipt, TransportError> {
        self.classify_mutation_status(
            response,
            201,
            command_id,
            MutationMessages {
                rejected: "Microsoft Graph rejected the chat message",
                unconfirmed: "Microsoft Graph could not confirm chat message delivery",
            },
        )
    }
}

fn chat_messages_url(cloud: GraphCloud, chat_id: &str) -> Result<Url, TransportError> {
    reject_dot_segment(chat_id)?;
    cloud
        .graph_route(&["chats", chat_id, "messages"], None)
        .map_err(|_| {
            TransportError::Unavailable("could not prepare the Microsoft Graph chat route".into())
        })
}

fn map_message_http_error(error: GraphHttpError) -> TransportError {
    map_mutation_http_error(
        error,
        "Microsoft Graph could not confirm chat message delivery",
        "Microsoft Graph chat delivery is unavailable",
    )
}

#[derive(Serialize)]
struct CreateChatMessage<'a> {
    body: WireBodyPayload<'a>,
    importance: &'static str,
    /// Absent for every message without a mention, so those keep their exact
    /// earlier wire form.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mentions: Vec<MentionPayload<'a>>,
    /// Absent for every message without a file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentPayload<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentPayload<'a> {
    id: &'a str,
    content_type: &'static str,
    content_url: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionPayload<'a> {
    id: usize,
    mention_text: &'a str,
    mentioned: MentionedPayload<'a>,
}

#[derive(Serialize)]
struct MentionedPayload<'a> {
    user: MentionedUser<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MentionedUser<'a> {
    display_name: &'a str,
    id: &'a str,
    user_identity_type: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireBodyPayload<'a> {
    content_type: &'static str,
    content: &'a str,
}

#[cfg(test)]
mod tests {
    use super::{GraphHttpError, GraphHttpStatus, Microsoft365Transport};
    use crate::integrations::ComposedBody;
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::rich_html::MAX_REMOTE_BODY_BYTES as MAX_REMOTE_MESSAGE_BYTES;
    use crate::integrations::microsoft365::{GraphHttpClient, GraphJsonRequest};
    use crate::integrations::{
        AccountIdentity, CommandDisposition, ConnectionStatus, ProviderResourceRef,
        RemoteChatMessageImportance, TransportError, WorkspaceCommand, WorkspaceTransport,
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

    fn accepted_response() -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status: 201,
            retry_after_seconds: None,
        })
    }

    fn remote_message(chat: &str, body: &str) -> WorkspaceCommand {
        remote_message_with_importance(chat, body, RemoteChatMessageImportance::Normal)
    }

    fn remote_message_with_importance(
        chat: &str,
        body: &str,
        importance: RemoteChatMessageImportance,
    ) -> WorkspaceCommand {
        WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::microsoft365(chat),
            body: body.into(),
            importance,
            attachment: None,
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
    fn sends_text_to_the_encoded_chat_message_route_without_reading_a_response_body() {
        let (client, state) = RecordingPostClient::new([accepted_response()]);
        let mut transport = connected_transport(Box::new(client));

        let receipt = transport
            .dispatch(remote_message("19:chat id@thread.v2", "Hello \"world\""))
            .expect("201 must acknowledge the remote message");

        assert_eq!(receipt.command_id, "microsoft365:chat-message");
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
            ["v1.0", "chats", "19:chat%20id@thread.v2", "messages"]
        );
        assert!(!posts[0].url.contains("chat id"));
        assert_eq!(posts[0].content_type, "application/json");
        let body: Value =
            serde_json::from_slice(&posts[0].body).expect("the Graph body must be valid JSON");
        assert_eq!(body["body"]["contentType"], "text");
        assert_eq!(body["body"]["content"], "Hello \"world\"");
        assert_eq!(body["importance"], "normal");
    }

    #[test]
    fn plain_send_wire_is_unchanged_byte_for_byte() {
        let (client, state) = RecordingPostClient::new([accepted_response()]);
        let mut transport = connected_transport(Box::new(client));
        let text = "a < b & \"c\"\n\n  indented **not bold";
        transport
            .dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::microsoft365("19:c@thread.v2"),
                body: crate::integrations::ComposedBody::from_markdown_lite(text),
                importance: RemoteChatMessageImportance::Normal,
                attachment: None,
            })
            .expect("plain send");
        let posts = state.posts.lock().expect("posts");
        assert_eq!(
            String::from_utf8(posts[0].body.clone()).expect("utf8"),
            // The pre-rich wire form, field order included.
            "{\"body\":{\"contentType\":\"text\",\"content\":\"a < b & \\\"c\\\"\\n\\n  indented **not bold\"},\"importance\":\"normal\"}"
        );
    }

    #[test]
    fn rich_send_uses_html_with_escaped_allow_listed_markup() {
        let (client, state) = RecordingPostClient::new([accepted_response()]);
        let mut transport = connected_transport(Box::new(client));
        transport
            .dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::microsoft365("19:c@thread.v2"),
                body: crate::integrations::ComposedBody::from_markdown_lite(
                    "**hi** <b>x</b> [d](https://example.com)",
                ),
                importance: RemoteChatMessageImportance::High,
                attachment: None,
            })
            .expect("rich send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert_eq!(body["body"]["contentType"], "html");
        assert_eq!(
            body["body"]["content"],
            "<p><strong>hi</strong> &lt;b&gt;x&lt;/b&gt; <a href=\"https://example.com/\">d</a></p>"
        );
        assert_eq!(body["importance"], "high");
    }

    const FILE_GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";
    const FILE_URL: &str = "https://contoso-my.sharepoint.com/personal/ada/Documents/Microsoft%20Teams%20Chat%20Files/q3.pdf";

    /// A transport that holds one uploaded file, and its opaque handle.
    fn transport_with_upload() -> (
        Microsoft365Transport,
        Arc<RecordingPostState>,
        crate::integrations::UploadedFileHandle,
    ) {
        let (client, state) = RecordingPostClient::new([accepted_response(), accepted_response()]);
        let mut transport = connected_transport(Box::new(client));
        let id = transport
            .uploads
            .register("q3.pdf".into(), FILE_GUID.into(), FILE_URL.into());
        let handle = crate::integrations::UploadedFileHandle::new(
            id,
            "q3.pdf".into(),
            5,
            FILE_GUID.into(),
            false,
        );
        (transport, state, handle)
    }

    fn send_with(
        transport: &mut Microsoft365Transport,
        body: crate::integrations::ComposedBody,
        attachment: Option<crate::integrations::UploadedFileHandle>,
    ) -> Result<crate::integrations::CommandReceipt, TransportError> {
        transport.dispatch(WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::microsoft365("19:c@thread.v2"),
            body,
            importance: RemoteChatMessageImportance::High,
            attachment,
        })
    }

    #[test]
    fn a_file_send_is_html_with_the_marker_and_one_reference_attachment() {
        let (mut transport, state, handle) = transport_with_upload();
        send_with(
            &mut transport,
            crate::integrations::ComposedBody::from_markdown_lite("Here is **the** budget"),
            Some(handle),
        )
        .expect("a file send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert_eq!(body["body"]["contentType"], "html");
        assert_eq!(
            body["body"]["content"],
            format!(
                "<p>Here is <strong>the</strong> budget</p><attachment id=\"{FILE_GUID}\"></attachment>"
            )
        );
        assert_eq!(
            body["attachments"],
            serde_json::json!([{
                "id": FILE_GUID,
                "contentType": "reference",
                "contentUrl": FILE_URL,
                "name": "q3.pdf"
            }])
        );
        assert_eq!(body["importance"], "high");
        assert!(body.get("mentions").is_none());
    }

    #[test]
    fn a_file_with_no_text_is_just_the_marker() {
        let (mut transport, state, handle) = transport_with_upload();
        send_with(
            &mut transport,
            crate::integrations::ComposedBody::plain(""),
            Some(handle),
        )
        .expect("a file-only send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert_eq!(body["body"]["contentType"], "html");
        assert_eq!(
            body["body"]["content"],
            format!("<attachment id=\"{FILE_GUID}\"></attachment>")
        );
    }

    #[test]
    fn a_plain_send_still_has_no_attachments_field_and_empty_text_is_refused() {
        let (mut transport, state, _) = transport_with_upload();
        send_with(
            &mut transport,
            crate::integrations::ComposedBody::plain("hello"),
            None,
        )
        .expect("a plain send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert!(body.get("attachments").is_none());
        drop(posts);
        assert!(matches!(
            send_with(
                &mut transport,
                crate::integrations::ComposedBody::plain(""),
                None
            ),
            Err(TransportError::InvalidCommand(_))
        ));
    }

    #[test]
    fn a_handle_this_session_did_not_issue_attaches_nothing_and_posts_nothing() {
        let (mut transport, state, handle) = transport_with_upload();
        let foreign = crate::integrations::UploadedFileHandle::new(
            handle.id() ^ 1,
            "q3.pdf".into(),
            5,
            FILE_GUID.into(),
            false,
        );
        let wrong_reference = crate::integrations::UploadedFileHandle::new(
            handle.id(),
            "q3.pdf".into(),
            5,
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".into(),
            false,
        );
        for stale in [foreign, wrong_reference] {
            assert!(matches!(
                send_with(
                    &mut transport,
                    crate::integrations::ComposedBody::plain("x"),
                    Some(stale)
                ),
                Err(TransportError::InvalidCommand(_))
            ));
        }
        // Reauthentication drops every handle.
        transport.record_error(&TransportError::ReauthenticationRequired);
        assert!(matches!(
            send_with(
                &mut transport,
                crate::integrations::ComposedBody::plain("x"),
                Some(handle)
            ),
            Err(TransportError::InvalidCommand(_))
        ));
        assert!(state.posts.lock().expect("posts").is_empty());
    }

    #[test]
    fn an_ambiguous_file_send_is_never_replayed() {
        let (client, state) = RecordingPostClient::new([Ok(GraphHttpStatus {
            status: 503,
            retry_after_seconds: None,
        })]);
        let mut transport = connected_transport(Box::new(client));
        let id = transport
            .uploads
            .register("q3.pdf".into(), FILE_GUID.into(), FILE_URL.into());
        let handle = crate::integrations::UploadedFileHandle::new(
            id,
            "q3.pdf".into(),
            5,
            FILE_GUID.into(),
            false,
        );
        assert!(matches!(
            send_with(
                &mut transport,
                crate::integrations::ComposedBody::plain("x"),
                Some(handle)
            ),
            Err(TransportError::Unavailable(_))
        ));
        assert_eq!(state.posts.lock().expect("posts").len(), 1);
    }

    const MENTION_CHAT: &str = "19:chat@thread.v2";

    /// A transport that has read People for `MENTION_CHAT`, and the handles of
    /// Ada, Bob and Eve in that roster.
    fn transport_with_roster() -> (
        Microsoft365Transport,
        Arc<RecordingPostState>,
        [crate::integrations::MentionHandle; 3],
    ) {
        let (client, state) = RecordingPostClient::new([
            accepted_response(),
            accepted_response(),
            accepted_response(),
        ]);
        let mut transport = connected_transport(Box::new(client));
        let handles = transport.member_ids.remember_roster(
            MENTION_CHAT,
            vec![
                (Some("id-ada".into()), "Ada \"A\" <Lovelace>".into()),
                (Some("id-bob".into()), "Bob".into()),
                (Some("id-eve".into()), "Eve".into()),
            ],
        );
        let handles = [
            handles[0].expect("ada"),
            handles[1].expect("bob"),
            handles[2].expect("eve"),
        ];
        (transport, state, handles)
    }

    fn send_composed(
        transport: &mut Microsoft365Transport,
        chat: &str,
        draft: &str,
        people: &[(crate::integrations::MentionHandle, &str)],
    ) -> Result<crate::integrations::CommandReceipt, TransportError> {
        let picked: Vec<_> = people
            .iter()
            .map(|(handle, name)| crate::integrations::DraftMention {
                handle: *handle,
                display: (*name).into(),
            })
            .collect();
        transport.dispatch(WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::microsoft365(chat),
            body: ComposedBody::compose(draft, &picked),
            importance: RemoteChatMessageImportance::Normal,
            attachment: None,
        })
    }

    #[test]
    fn exact_html_and_json_for_0_1_and_3_mentions() {
        let (mut transport, state, [ada, bob, eve]) = transport_with_roster();
        // Zero mentions: the plain `text` wire form, with no `mentions` key.
        send_composed(&mut transport, MENTION_CHAT, "no one here", &[]).expect("plain");
        send_composed(&mut transport, MENTION_CHAT, "@Bob hi", &[(bob, "Bob")]).expect("one");
        send_composed(
            &mut transport,
            MENTION_CHAT,
            "@Bob @Eve and @Bob again",
            &[(bob, "Bob"), (eve, "Eve")],
        )
        .expect("three");
        let _ = ada;
        let posts = state.posts.lock().expect("posts");
        let wire = |index: usize| String::from_utf8(posts[index].body.clone()).expect("utf8");
        assert_eq!(
            wire(0),
            r#"{"body":{"contentType":"text","content":"no one here"},"importance":"normal"}"#
        );
        assert_eq!(
            wire(1),
            concat!(
                r#"{"body":{"contentType":"html","content":"<p><at id=\"0\">Bob</at> hi</p>"},"importance":"normal","#,
                r#""mentions":[{"id":0,"mentionText":"Bob","mentioned":{"user":{"displayName":"Bob","id":"id-bob","userIdentityType":"aadUser"}}}]}"#
            )
        );
        let three: Value = serde_json::from_str(&wire(2)).expect("json");
        assert_eq!(
            three["body"]["content"],
            "<p><at id=\"0\">Bob</at> <at id=\"1\">Eve</at> and <at id=\"2\">Bob</at> again</p>"
        );
        let mentions = three["mentions"].as_array().expect("array");
        assert_eq!(mentions.len(), 3);
        let ids: Vec<_> = mentions
            .iter()
            .map(|m| m["mentioned"]["user"]["id"].clone())
            .collect();
        assert_eq!(ids, ["id-bob", "id-eve", "id-bob"]);
        for (index, mention) in mentions.iter().enumerate() {
            assert_eq!(mention["id"], index);
        }
    }

    #[test]
    fn escaping_in_mention_text() {
        let (mut transport, state, [ada, ..]) = transport_with_roster();
        let name = "Ada \"A\" <Lovelace>";
        send_composed(
            &mut transport,
            MENTION_CHAT,
            &format!("hi @{name}"),
            &[(ada, name)],
        )
        .expect("send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert_eq!(
            body["body"]["content"],
            "<p>hi <at id=\"0\">Ada &quot;A&quot; &lt;Lovelace&gt;</at></p>"
        );
        // The JSON carries the raw name; only the HTML is escaped.
        assert_eq!(body["mentions"][0]["mentionText"], name);
        assert_eq!(
            body["mentions"][0]["mentioned"]["user"]["displayName"],
            name
        );
    }

    #[test]
    fn cross_chat_handle_rejected_before_network() {
        let (mut transport, state, [_, bob, _]) = transport_with_roster();
        // Bob's handle belongs to MENTION_CHAT, not to this chat.
        transport.member_ids.remember_roster(
            "19:other@thread.v2",
            vec![(Some("id-x".into()), "Xan".into())],
        );
        let error = send_composed(
            &mut transport,
            "19:other@thread.v2",
            "@Bob hi",
            &[(bob, "Bob")],
        )
        .expect_err("another chat's handle");
        assert!(matches!(error, TransportError::InvalidCommand(_)));
        assert_eq!(state.posts.lock().expect("posts").len(), 0);
    }

    #[test]
    fn unknown_handle_rejected_before_network() {
        let (mut transport, state, _) = transport_with_roster();
        let stranger = crate::integrations::MentionHandle::for_test(0xDEAD_BEEF);
        for chat in [MENTION_CHAT, "19:no-roster@thread.v2"] {
            let error = send_composed(&mut transport, chat, "@Bob hi", &[(stranger, "Bob")])
                .expect_err("unknown handle");
            assert!(matches!(error, TransportError::InvalidCommand(_)));
        }
        assert_eq!(state.posts.lock().expect("posts").len(), 0);
    }

    #[test]
    fn display_mismatch_rejected() {
        let (mut transport, state, [_, bob, _]) = transport_with_roster();
        let error = send_composed(
            &mut transport,
            MENTION_CHAT,
            "@Robert hi",
            &[(bob, "Robert")],
        )
        .expect_err("the composer's name must be the remembered name");
        assert!(matches!(error, TransportError::InvalidCommand(_)));
        assert_eq!(state.posts.lock().expect("posts").len(), 0);
    }

    #[test]
    fn at_most_twenty_mentions_are_sent_and_the_rest_stay_text() {
        let (mut transport, state, [_, bob, _]) = transport_with_roster();
        let draft = "@Bob ".repeat(25);
        send_composed(&mut transport, MENTION_CHAT, draft.trim(), &[(bob, "Bob")]).expect("send");
        let posts = state.posts.lock().expect("posts");
        let body: Value = serde_json::from_slice(&posts[0].body).expect("json");
        assert_eq!(body["mentions"].as_array().expect("array").len(), 20);
        let content = body["body"]["content"].as_str().expect("content");
        assert_eq!(content.matches("<at ").count(), 20);
        assert!(content.ends_with("@Bob @Bob @Bob @Bob @Bob</p>"));
    }

    #[test]
    fn no_user_id_in_command_debug() {
        let (_, _, [_, bob, _]) = transport_with_roster();
        let picked = [crate::integrations::DraftMention {
            handle: bob,
            display: "Bob".into(),
        }];
        let command = WorkspaceCommand::SendChatMessage {
            chat: ProviderResourceRef::microsoft365(MENTION_CHAT),
            body: ComposedBody::compose("@Bob", &picked),
            importance: RemoteChatMessageImportance::Normal,
            attachment: None,
        };
        let debug = format!("{command:?}");
        assert!(!debug.contains("id-bob"));
        assert!(debug.contains("MentionHandle(..)"));
    }

    #[test]
    fn sends_exact_graph_high_and_urgent_importance_values() {
        let (client, state) = RecordingPostClient::new([accepted_response(), accepted_response()]);
        let mut transport = connected_transport(Box::new(client));

        transport
            .dispatch(remote_message_with_importance(
                "19:chat@thread.v2",
                "Needs attention",
                RemoteChatMessageImportance::High,
            ))
            .expect("high-priority message must be accepted");
        transport
            .dispatch(remote_message_with_importance(
                "19:chat@thread.v2",
                "Needs immediate attention",
                RemoteChatMessageImportance::Urgent,
            ))
            .expect("urgent message must be accepted");

        let posts = state
            .posts
            .lock()
            .expect("recorded post list must not be poisoned");
        assert_eq!(posts.len(), 2);
        let importance = posts
            .iter()
            .map(|post| {
                serde_json::from_slice::<Value>(&post.body)
                    .expect("the Graph body must be valid JSON")["importance"]
                    .as_str()
                    .expect("importance must be a JSON string")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(importance, ["high", "urgent"]);
    }

    #[test]
    fn local_preview_targets_are_rejected_before_posting() {
        let (client, state) = RecordingPostClient::new([accepted_response()]);
        let mut transport = connected_transport(Box::new(client));

        let error = transport
            .dispatch(WorkspaceCommand::SendChatMessage {
                chat: ProviderResourceRef::local_preview("fixture-chat"),
                body: "This must remain local".into(),
                importance: RemoteChatMessageImportance::Normal,
                attachment: None,
            })
            .expect_err("local refs cannot be sent to Graph");

        assert!(matches!(error, TransportError::Unsupported(_)));
        assert_eq!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .len(),
            0
        );
    }

    #[test]
    fn dot_segment_chat_ids_are_rejected_before_building_a_route() {
        for id in [".", "..", " .. "] {
            assert!(matches!(
                super::chat_messages_url(GraphCloud::Global, id),
                Err(TransportError::Unsupported(_))
            ));
        }
        assert!(super::chat_messages_url(GraphCloud::Global, "19:abc@thread.v2").is_ok());
    }

    #[test]
    fn message_statuses_preserve_or_invalidate_connection_state_as_required() {
        let cases = [
            (401, None, true, "authentication must be renewed"),
            (403, None, false, "authorization"),
            (429, Some(17), false, "throttled"),
            (500, None, false, "could not confirm"),
            (400, None, false, "rejected"),
            (404, None, false, "rejected"),
            (409, None, false, "rejected"),
            (413, None, false, "rejected"),
            (422, None, false, "rejected"),
        ];

        for (status, retry_after_seconds, identity_cleared, error_fragment) in cases {
            let (client, _state) = RecordingPostClient::new([Ok(GraphHttpStatus {
                status,
                retry_after_seconds,
            })]);
            let mut transport = connected_transport(Box::new(client));

            let error = transport
                .dispatch(remote_message(
                    "19:private-chat@thread.v2",
                    "Sensitive message body",
                ))
                .expect_err("non-201 statuses cannot acknowledge delivery");

            assert!(error.to_string().contains(error_fragment));
            assert!(!error.to_string().contains("private-chat"));
            assert!(!error.to_string().contains("Sensitive message body"));
            assert!(!error.to_string().contains("sensitive-access-token"));
            if status == 429 {
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
    fn ambiguous_network_error_and_remote_body_limit_never_post_or_leak_content() {
        let (client, state) = RecordingPostClient::new([Err(GraphHttpError::Network)]);
        let mut transport = connected_transport(Box::new(client));
        let error = transport
            .dispatch(remote_message("19:chat@thread.v2", "confidential body"))
            .expect_err("network delivery status is ambiguous");
        assert!(matches!(error, TransportError::Unavailable(_)));
        assert!(!error.to_string().contains("confidential body"));

        let oversized = "x".repeat(MAX_REMOTE_MESSAGE_BYTES + 1);
        let error = transport
            .dispatch(remote_message("19:chat@thread.v2", &oversized))
            .expect_err("remote payloads must be bounded");
        assert!(matches!(error, TransportError::InvalidCommand(_)));
        assert_eq!(
            state
                .posts
                .lock()
                .expect("recorded post list must not be poisoned")
                .len(),
            1,
            "oversized payload must not be posted"
        );
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url =
                super::chat_messages_url(cloud, "c").unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}
