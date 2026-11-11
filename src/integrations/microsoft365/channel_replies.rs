//! Selection-scoped Microsoft Graph channel reply normalization.
//!
//! Root posts and their replies deliberately use separate Graph routes. This
//! module reads a single bounded page only after the user selects one visible
//! root post. It owns the exact route, provider target validation, response
//! bounds, and HTML-to-inert-text conversion; replies, cursors, and raw Graph
//! DTOs never leave the integration boundary.

use super::GraphSession;
use super::chats::normalize_graph_message_body;
use super::reactions::{deserialize_reactions, normalize_reaction_summaries};
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderResourceRef, RemoteChannelRepliesProjection, RemoteChannelReply,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use std::collections::BTreeSet;
use url::Url;

const MAX_CHANNEL_REPLIES_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_CHANNEL_REPLIES: usize = 50;
const CHANNEL_REPLIES_PAGE_SIZE: usize = 50;
const MAX_AUTHOR_BYTES: usize = 256;
const MAX_TIMESTAMP_BYTES: usize = 96;

/// A bounded reply collection plus presentation-safe notices. Provider URLs,
/// cursor values, raw HTML, and DTOs cannot escape through this result.
struct ChannelRepliesReadResult {
    replies: Vec<RemoteChannelReply>,
    notices: Vec<String>,
}

/// Reply-read failures remain intentionally categorical. In particular, a
/// Graph error body must never be displayed or persisted by feature code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelRepliesReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl ChannelRepliesReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph channel reply synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one selected root post's bounded reply feed. This remains a
    /// separate interest from channel root posts so no directory or root-post
    /// refresh can accidentally fan out into every thread.
    pub(super) fn poll_channel_replies(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_replies_with_cancellation(team, channel, post, None)
    }

    /// Lets the controller stop an obsolete selected-post read before it can
    /// publish a reply feed for another root post or profile.
    pub(super) fn poll_channel_replies_cancellable(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_replies_with_cancellation(team, channel, post, Some(cancellation))
    }

    fn poll_channel_replies_with_cancellation(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let (team_id, channel_id, post_id) = selected_reply_identifiers(&team, &channel, &post)?;
        let completed = SyncInterest::ChannelReplies {
            team: team.clone(),
            channel: channel.clone(),
            post: post.clone(),
        };
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }

        let current_account_id = self
            .identity
            .as_ref()
            .map(|identity| identity.account_id.as_str());
        match fetch_channel_replies_for_account(
            &self.session,
            team_id,
            channel_id,
            post_id,
            current_account_id,
            cancellation,
        ) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: Some(RemoteChannelRepliesProjection::microsoft365(
                    team,
                    channel,
                    post,
                    result.replies,
                )?),
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(ChannelRepliesReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Preserve the selected root post while clearing its old
                // replies. A 403 can be a permission or membership rule, so
                // the feature receives a neutral per-post access state.
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: Some(RemoteChannelRepliesProjection::access_denied(
                    team, channel, post,
                )?),
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this post's replies. Verify delegated ChannelMessage.Read.All access and your channel membership, then reconnect."
                        .into(),
                ],
            }),
            Err(ChannelRepliesReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(ChannelRepliesReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn selected_reply_identifiers<'a>(
    team: &'a ProviderResourceRef,
    channel: &'a ProviderResourceRef,
    post: &'a ProviderResourceRef,
) -> Result<(&'a str, &'a str, &'a str), TransportError> {
    let team_id = team.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
    })?;
    let channel_id = channel.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview channel".into())
    })?;
    let post_id = post.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported(
            "Microsoft Graph cannot target a local preview channel root post".into(),
        )
    })?;
    if normalized_identifier(team_id).as_deref() != Some(team_id)
        || normalized_identifier(channel_id).as_deref() != Some(channel_id)
        || normalized_identifier(post_id).as_deref() != Some(post_id)
    {
        return Err(TransportError::InvalidCommand(
            "Microsoft Graph channel reply selection contains an invalid provider identifier"
                .into(),
        ));
    }
    Ok((team_id, channel_id, post_id))
}

#[cfg(test)]
fn fetch_channel_replies(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    post_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChannelRepliesReadResult, ChannelRepliesReadFailure> {
    fetch_channel_replies_for_account(session, team_id, channel_id, post_id, None, cancellation)
}

/// The account ID stays adapter-private and only derives whether the signed-in
/// account selected an exact supported reaction value in this bounded feed.
fn fetch_channel_replies_for_account(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    post_id: &str,
    current_account_id: Option<&str>,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChannelRepliesReadResult, ChannelRepliesReadFailure> {
    check_cancelled(cancellation)?;
    let url = channel_replies_url(session.cloud(), team_id, channel_id, post_id)?;
    let response = get_json(session, &url, cancellation)?;
    let page = decode_collection(&response.body)?;
    let truncated = page.next_link.is_some() || page.value.len() > MAX_CHANNEL_REPLIES;
    let mut seen = BTreeSet::new();
    let mut replies = Vec::with_capacity(page.value.len().min(MAX_CHANNEL_REPLIES));
    let mut skipped_replies = 0_usize;

    for reply in page.value.into_iter().take(MAX_CHANNEL_REPLIES) {
        check_cancelled(cancellation)?;
        let Some(reply_id) = reply.id.as_deref().and_then(normalized_identifier) else {
            skipped_replies = skipped_replies.saturating_add(1);
            continue;
        };
        let Some(reply_to_id) = reply.reply_to_id.as_deref().and_then(normalized_identifier) else {
            skipped_replies = skipped_replies.saturating_add(1);
            continue;
        };
        if reply_to_id != post_id || !seen.insert(reply_id) {
            skipped_replies = skipped_replies.saturating_add(1);
            continue;
        }
        if let Some(reply) = normalize_reply(reply, current_account_id) {
            replies.push(reply);
        } else {
            skipped_replies = skipped_replies.saturating_add(1);
        }
    }

    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Microsoft 365 reply sync is showing at most {MAX_CHANNEL_REPLIES} replies for this post."
        ));
    }
    if skipped_replies > 0 {
        notices.push(
            "Microsoft Graph returned some replies without usable identifiers or root-post bindings; they were skipped."
                .into(),
        );
    }
    check_cancelled(cancellation)?;
    Ok(ChannelRepliesReadResult { replies, notices })
}

fn channel_replies_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
    post_id: &str,
) -> Result<Url, ChannelRepliesReadFailure> {
    let mut url = cloud
        .graph_route(
            &[
                "teams", team_id, "channels", channel_id, "messages", post_id, "replies",
            ],
            None,
        )
        .map_err(|_| ChannelRepliesReadFailure::Unavailable)?;
    // The replies route supports `$top` only (maximum 50). Do not add
    // `$select`, `$orderby`, or `$expand`, and never follow the opaque cursor
    // from a response: each selected-post read is exactly one bounded page.
    url.query_pairs_mut()
        .append_pair("$top", &CHANNEL_REPLIES_PAGE_SIZE.to_string());
    Ok(url)
}

fn normalize_reply(
    reply: GraphChannelReply,
    current_account_id: Option<&str>,
) -> Option<RemoteChannelReply> {
    let id = reply.id.as_deref().and_then(normalized_identifier)?;
    let deleted = reply.deleted_date_time.is_some();
    let author = sender_name(reply.from.as_ref());
    let created_at = reply
        .created_date_time
        .as_deref()
        .map(|value| safe_display(value, MAX_TIMESTAMP_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unknown time".into());
    let plain_text = if deleted {
        "This reply was deleted.".into()
    } else {
        normalize_graph_message_body(
            reply
                .body
                .as_ref()
                .and_then(|body| body.content_type.as_deref()),
            reply.body.as_ref().and_then(|body| body.content.as_deref()),
        )
    };
    let reactions = normalize_reaction_summaries(
        &reply.reactions,
        current_account_id,
        |reaction| reaction.reaction_type.as_deref(),
        |reaction| {
            reaction
                .user
                .as_ref()
                .and_then(|identity| identity.user.as_ref())
                .and_then(|user| user.id.as_deref())
        },
    );
    RemoteChannelReply::microsoft365(
        ProviderResourceRef::microsoft365(id),
        author,
        created_at,
        plain_text,
        deleted,
        reply.last_edited_date_time.is_some(),
        reactions,
    )
    .ok()
}

fn sender_name(from: Option<&GraphMessageFrom>) -> String {
    let identity = from.and_then(|from| {
        from.user
            .as_ref()
            .or(from.application.as_ref())
            .or(from.device.as_ref())
    });
    identity
        .and_then(|identity| identity.display_name.as_deref())
        .map(|value| safe_display(value, MAX_AUTHOR_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Microsoft 365".into())
}

fn get_json(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphHttpResponse, ChannelRepliesReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_CHANNEL_REPLIES_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_response_status(&response)?;
    Ok(response)
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), ChannelRepliesReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(ChannelRepliesReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> ChannelRepliesReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => ChannelRepliesReadFailure::Unavailable,
    }
}

fn validate_response_status(response: &GraphHttpResponse) -> Result<(), ChannelRepliesReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(ChannelRepliesReadFailure::ReauthenticationRequired),
        403 => Err(ChannelRepliesReadFailure::AccessDenied),
        429 => Err(ChannelRepliesReadFailure::Throttled(
            response.retry_after_seconds,
        )),
        _ => Err(ChannelRepliesReadFailure::Unavailable),
    }
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection(body: &[u8]) -> Result<GraphCollection, ChannelRepliesReadFailure> {
    serde_json::from_slice::<GraphCollection>(body)
        .map_err(|_| ChannelRepliesReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

#[derive(Deserialize)]
struct GraphCollection {
    value: Vec<GraphChannelReply>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChannelReply {
    id: Option<String>,
    reply_to_id: Option<String>,
    from: Option<GraphMessageFrom>,
    created_date_time: Option<String>,
    last_edited_date_time: Option<String>,
    deleted_date_time: Option<String>,
    body: Option<GraphItemBody>,
    #[serde(default, deserialize_with = "deserialize_reactions")]
    reactions: Vec<GraphMessageReaction>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageReaction {
    reaction_type: Option<String>,
    user: Option<GraphMessageFrom>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageFrom {
    user: Option<GraphIdentity>,
    application: Option<GraphIdentity>,
    device: Option<GraphIdentity>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphIdentity {
    id: Option<String>,
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphItemBody {
    content_type: Option<String>,
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        CHANNEL_REPLIES_PAGE_SIZE, ChannelRepliesReadFailure, GraphHttpError, GraphHttpResponse,
        MAX_CHANNEL_REPLIES_RESPONSE_BYTES, Microsoft365Transport, channel_replies_url,
        fetch_channel_replies, fetch_channel_replies_for_account, selected_reply_identifiers,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, ProviderResourceRef, SyncInterest,
        TransportCancellation, TransportError, WorkspaceTransport,
    };
    use crate::shared::reaction::ReactionKind;
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use url::Url;

    struct TestHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requested_urls: Mutex<Vec<String>>,
        requested_body_limits: Mutex<Vec<u64>>,
    }

    impl TestHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
                requested_body_limits: Mutex::new(Vec::new()),
            }
        }
    }

    impl GraphHttpClient for TestHttpClient {
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
            self.requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .push(url.as_str().into());
            self.requested_body_limits
                .lock()
                .expect("test response-limit list must not be poisoned")
                .push(max_response_bytes);
            self.responses
                .lock()
                .expect("test response queue must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    struct CancellingHttpClient {
        cancellation: TransportCancellation,
        requests: AtomicUsize,
    }

    impl GraphHttpClient for CancellingHttpClient {
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
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.cancellation.cancel();
            response(200, r#"{"value":[]}"#)
        }
    }

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn selection() -> (
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    ) {
        (
            ProviderResourceRef::microsoft365("team-1"),
            ProviderResourceRef::microsoft365("channel-1"),
            ProviderResourceRef::microsoft365("post-1"),
        )
    }

    #[test]
    fn selected_post_replies_use_only_the_documented_bounded_route_and_safe_fields() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":"reply-1","replyToId":"post-1","from":{"user":{"displayName":" Ada\nLovelace "}},"createdDateTime":"2026-11-06T10:30:00Z","lastEditedDateTime":"2026-11-06T10:32:00Z","body":{"contentType":"html","content":"<p>Ready &amp; <strong>safe</strong><script>ignore()</script></p>"}}]}"#,
        )]);

        let result = fetch_channel_replies(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            "post-1",
            None,
        )
        .expect("valid channel replies must normalize");

        assert_eq!(result.replies.len(), 1);
        let reply = &result.replies[0];
        assert_eq!(reply.author(), "Ada Lovelace");
        assert_eq!(reply.created_at(), "2026-11-06T10:30:00Z");
        assert_eq!(reply.plain_text(), "Ready & safe");
        assert!(reply.is_edited());
        assert!(!reply.is_deleted());
        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        let url = Url::parse(&requested[0]).expect("channel replies request must be a URL");
        assert_eq!(
            url.path(),
            "/v1.0/teams/team-1/channels/channel-1/messages/post-1/replies"
        );
        let query = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.len(), 1);
        assert_eq!(query[0].0, "$top");
        assert_eq!(query[0].1, CHANNEL_REPLIES_PAGE_SIZE.to_string());
        assert_eq!(
            client
                .requested_body_limits
                .lock()
                .expect("test response-limit list must not be poisoned")
                .as_slice(),
            [MAX_CHANNEL_REPLIES_RESPONSE_BYTES]
        );
    }

    #[test]
    fn reply_reactions_normalize_only_the_supported_palette() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":"reply-1","replyToId":"post-1","createdDateTime":"2026-11-06T10:30:00Z","body":{"contentType":"text","content":"Visible"},"reactions":[{"reactionType":"👍","user":{"user":{"id":"other-user","displayName":"Other"}}},{"reactionType":"❤","user":{"user":{"id":"current-user"}}},{"reactionType":"👍","user":{"user":{"id":"current-user","displayName":"Current"}}},{"reactionType":"👍","user":{"application":{"id":"current-user"}}},{"reactionType":"😡","user":{"user":{"id":"other-user"}}},{"reactionType":"heart","user":{"user":{"id":"current-user"}}},{"reactionType":"custom","user":{"user":{"id":"current-user"}}}]}]}"#,
        )]);

        let result = fetch_channel_replies_for_account(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            "post-1",
            Some("current-user"),
            None,
        )
        .expect("reply reactions must normalize with the selected reply feed");

        assert_eq!(result.replies.len(), 1);
        let reply = &result.replies[0];
        assert_eq!(reply.reactions().len(), 3);
        assert_eq!(reply.reactions()[0].kind(), ReactionKind::Like);
        assert_eq!(reply.reactions()[0].count(), 3);
        assert!(reply.reactions()[0].reacted_by_current_user());
        // The legacy `heart` name counts with the canonical `❤`.
        assert_eq!(reply.reactions()[1].kind(), ReactionKind::Heart);
        assert_eq!(reply.reactions()[1].count(), 2);
        assert!(reply.reactions()[1].reacted_by_current_user());
        assert_eq!(reply.reactions()[2].kind(), ReactionKind::Angry);
        assert_eq!(reply.reactions()[2].count(), 1);
        assert!(!reply.reactions()[2].reacted_by_current_user());
    }

    #[test]
    fn selected_post_replies_stop_at_one_page_and_report_truncation() {
        let values = (0..51)
            .map(|index| {
                format!(
                    r#"{{"id":"reply-{index}","replyToId":"post-1","createdDateTime":"2026-11-06T10:30:00Z","body":{{"contentType":"text","content":"Reply {index}"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let client = TestHttpClient::new([response(
            200,
            &format!(
                r#"{{"value":[{values}],"@odata.nextLink":"https://example.test/not-followed"}}"#
            ),
        )]);

        let result = fetch_channel_replies(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            "post-1",
            None,
        )
        .expect("the first reply page must normalize");

        assert_eq!(result.replies.len(), 50);
        assert!(result.notices[0].contains("at most 50 replies"));
        assert_eq!(
            client
                .requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn malformed_or_mismatched_replies_are_skipped() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":" ","replyToId":"post-1","body":{"content":"Missing ID"}},{"id":"reply-1","replyToId":"another-post","body":{"content":"Wrong post"}},{"id":"reply-2","replyToId":"post-1","body":{"content":"Visible"}}]}"#,
        )]);

        let result = fetch_channel_replies(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            "post-1",
            None,
        )
        .expect("malformed replies must not fail the bounded listing");

        assert_eq!(result.replies.len(), 1);
        assert_eq!(result.replies[0].plain_text(), "Visible");
        assert!(result.notices[0].contains("root-post bindings"));
    }

    #[test]
    fn cancellation_discards_a_completed_reply_response() {
        let cancellation = TransportCancellation::new();
        let client = CancellingHttpClient {
            cancellation: cancellation.clone(),
            requests: AtomicUsize::new(0),
        };

        let result = fetch_channel_replies(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            "post-1",
            Some(&cancellation),
        );

        assert!(matches!(result, Err(ChannelRepliesReadFailure::Cancelled)));
        assert_eq!(client.requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn access_denial_replaces_replies_without_losing_verified_identity() {
        let client =
            Box::new(TestHttpClient::new([response(403, "{}")])) as Box<dyn GraphHttpClient>;
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity.clone());
        let (team, channel, post) = selection();

        let delta = transport
            .poll(SyncInterest::ChannelReplies {
                team: team.clone(),
                channel: channel.clone(),
                post: post.clone(),
            })
            .expect("reply access denial must retain a verified identity");

        let projection = delta
            .remote_channel_replies
            .expect("an access-denied reply projection is expected");
        assert!(projection.is_access_denied());
        assert_eq!(projection.team(), team);
        assert_eq!(projection.channel(), channel);
        assert_eq!(projection.post(), post);
        assert!(projection.replies().is_empty());
        assert!(delta.notices[0].contains("ChannelMessage.Read.All"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(current) if current.account_id == "u1"
        ));
    }

    #[test]
    fn unauthenticated_reply_read_requires_reauthentication() {
        let client = Box::new(TestHttpClient::new([response(
            401,
            r#"{"error":"ignored"}"#,
        )])) as Box<dyn GraphHttpClient>;
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity);
        let (team, channel, post) = selection();

        let result = transport.poll(SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        });

        assert!(matches!(
            result,
            Err(TransportError::ReauthenticationRequired)
        ));
        assert!(transport.identity.is_none());
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::ReauthenticationRequired
        ));
    }

    #[test]
    fn throttled_reply_read_exposes_only_retry_metadata() {
        let client = Box::new(TestHttpClient::new([Ok(GraphHttpResponse {
            status: 429,
            retry_after_seconds: Some(17),
            body: br#"{"error":{"message":"provider detail must stay private"}}"#.to_vec(),
        })])) as Box<dyn GraphHttpClient>;
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let (team, channel, post) = selection();

        let result = transport.poll(SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        });

        assert!(matches!(
            result,
            Err(TransportError::Throttled {
                retry_after_seconds: Some(17)
            })
        ));
    }

    #[test]
    fn local_preview_targets_are_rejected_before_networking() {
        let (team, channel, _) = selection();
        let local_post = ProviderResourceRef::local_preview("post-1");

        assert!(selected_reply_identifiers(&team, &channel, &local_post).is_err());
    }

    #[test]
    fn built_in_reply_url_is_valid() {
        assert!(channel_replies_url(GraphCloud::Global, "team-1", "channel-1", "post-1").is_ok());
    }

    #[test]
    fn routes_use_the_configured_clouds_graph_host() {
        for cloud in GraphCloud::ALL {
            let url = super::channel_replies_url(cloud, "t", "c", "p")
                .unwrap_or_else(|_| panic!("route must build"));
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some(cloud.graph_host()));
            assert!(url.path().starts_with("/v1.0/"));
        }
    }
}

#[cfg(test)]
mod decode_collection_tests {
    use super::{ChannelRepliesReadFailure, decode_collection};

    #[test]
    fn decode_collection_reads_a_page_and_maps_malformed_bodies_to_unavailable() {
        let page =
            decode_collection(br#"{"value":[],"@odata.nextLink":"https://example.invalid/n"}"#)
                .expect("an empty page decodes");
        assert!(page.value.is_empty());
        assert_eq!(page.next_link.as_deref(), Some("https://example.invalid/n"));
        for body in [&b""[..], b"null", b"[]", b"{\"value\":3}", b"\xff\xfe"] {
            assert!(matches!(
                decode_collection(body),
                Err(ChannelRepliesReadFailure::Unavailable)
            ));
        }
    }
}
