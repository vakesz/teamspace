//! Selection-scoped Microsoft Graph channel root-post normalization.
//!
//! A Teams directory may contain many channels, so this module reads posts
//! only after the user selects one. It owns the precise Graph route, strict
//! continuation checks, response bounds, HTML-to-inert-text conversion, and
//! deliberately bounded typed current-user reaction summaries. The resulting
//! projection is session-only and deliberately excludes replies, attachments,
//! and every mutation path except the separately typed root-post reaction.

use super::GraphSession;
use super::chats::normalize_graph_message_body;
use super::reactions::{deserialize_reactions, normalize_reaction_summaries};
use super::text::{normalized_identifier, safe_display};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    ProviderResourceRef, RemoteChannelMessagesProjection, RemoteChannelPost,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use url::Url;

const MAX_CHANNEL_MESSAGES_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_CHANNEL_MESSAGE_PAGES: usize = 2;
const MAX_CHANNEL_ROOT_POSTS: usize = 50;
const CHANNEL_MESSAGE_PAGE_SIZE: usize = 25;
const MAX_AUTHOR_BYTES: usize = 256;
const MAX_TIMESTAMP_BYTES: usize = 96;
const MAX_SUBJECT_BYTES: usize = 512;

/// A bounded root-post collection and presentation-safe notices. Graph URLs,
/// continuation tokens, raw HTML, and DTOs do not leave this adapter.
struct ChannelMessagesReadResult {
    posts: Vec<RemoteChannelPost>,
    notices: Vec<String>,
}

/// Channel-message failures remain intentionally category-only. Provider error
/// bodies and endpoint details must never surface in app or feature state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelMessagesReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl ChannelMessagesReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph channel message synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads one selected channel's bounded root-post feed. This is separate
    /// from the Teams-directory poll because the current selection, rather
    /// than the whole tenant hierarchy, defines the permitted request.
    pub(super) fn poll_channel_messages(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_messages_with_cancellation(team, channel, None)
    }

    /// Lets the controller stop a stale selected-channel page sequence before
    /// it begins another request for a different selection or profile.
    pub(super) fn poll_channel_messages_cancellable(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_channel_messages_with_cancellation(team, channel, Some(cancellation))
    }

    fn poll_channel_messages_with_cancellation(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let (team_id, channel_id) = selected_channel_identifiers(&team, &channel)?;
        let completed = SyncInterest::ChannelMessages {
            team: team.clone(),
            channel: channel.clone(),
        };
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }

        let current_account_id = self
            .identity
            .as_ref()
            .map(|identity| identity.account_id.as_str());
        match fetch_channel_messages_for_account(
            &self.session,
            team_id,
            channel_id,
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
                remote_channel_messages: Some(RemoteChannelMessagesProjection::microsoft365(
                    team,
                    channel,
                    result.posts,
                )?),
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(ChannelMessagesReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Preserve the selected target while replacing prior posts so
                // the feature can show a truthful, per-channel access state.
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: Some(RemoteChannelMessagesProjection::access_denied(
                    team, channel,
                )?),
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this channel's posts. Verify delegated ChannelMessage.Read.All access and your channel membership, then reconnect."
                        .into(),
                ],
            }),
            Err(ChannelMessagesReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(ChannelMessagesReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }
}

fn selected_channel_identifiers<'a>(
    team: &'a ProviderResourceRef,
    channel: &'a ProviderResourceRef,
) -> Result<(&'a str, &'a str), TransportError> {
    let team_id = team.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview team".into())
    })?;
    let channel_id = channel.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview channel".into())
    })?;
    if normalized_identifier(team_id).as_deref() != Some(team_id)
        || normalized_identifier(channel_id).as_deref() != Some(channel_id)
    {
        return Err(TransportError::InvalidCommand(
            "Microsoft Graph channel selection contains an invalid provider identifier".into(),
        ));
    }
    Ok((team_id, channel_id))
}

#[cfg(test)]
fn fetch_channel_messages(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChannelMessagesReadResult, ChannelMessagesReadFailure> {
    fetch_channel_messages_for_account(session, team_id, channel_id, None, cancellation)
}

/// The account ID is integration-owned identity data, used solely to derive
/// whether the signed-in account has already selected an exact supported
/// reaction value. It never crosses the projection seam.
fn fetch_channel_messages_for_account(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    current_account_id: Option<&str>,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChannelMessagesReadResult, ChannelMessagesReadFailure> {
    check_cancelled(cancellation)?;
    let (messages, truncated, mut skipped_posts) =
        fetch_channel_message_pages(session, team_id, channel_id, cancellation)?;
    let mut posts = Vec::with_capacity(messages.len());
    for message in messages {
        check_cancelled(cancellation)?;
        if let Some(post) = normalize_post(message, current_account_id) {
            posts.push(post);
        } else {
            skipped_posts = skipped_posts.saturating_add(1);
        }
    }

    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Microsoft 365 channel sync is showing at most {MAX_CHANNEL_ROOT_POSTS} root posts for this channel."
        ));
    }
    if skipped_posts > 0 {
        notices.push(
            "Microsoft Graph returned some channel posts without usable identifiers; they were skipped."
                .into(),
        );
    }
    check_cancelled(cancellation)?;
    Ok(ChannelMessagesReadResult { posts, notices })
}

fn fetch_channel_message_pages(
    session: &GraphSession<'_>,
    team_id: &str,
    channel_id: &str,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphChannelMessage>, bool, usize), ChannelMessagesReadFailure> {
    let mut next = channel_messages_url(session.cloud(), team_id, channel_id)?;
    let expected_path = next.path().to_owned();
    let mut messages = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let mut skipped_posts = 0_usize;

    for page_index in 0..MAX_CHANNEL_MESSAGE_PAGES {
        let page: GraphCollection<GraphChannelMessage> =
            get_collection(session, &next, cancellation)?;
        for message in page.value {
            let Some(id) = message.id.as_deref().and_then(normalized_identifier) else {
                skipped_posts = skipped_posts.saturating_add(1);
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            messages.push(message);
            if messages.len() == MAX_CHANNEL_ROOT_POSTS {
                truncated = true;
                return Ok((messages, truncated, skipped_posts));
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok((messages, truncated, skipped_posts));
        };
        next = validated_graph_next_link(session.cloud(), &next_link, &expected_path)?;
        if page_index + 1 == MAX_CHANNEL_MESSAGE_PAGES {
            truncated = true;
        }
    }
    Ok((messages, truncated, skipped_posts))
}

fn channel_messages_url(
    cloud: GraphCloud,
    team_id: &str,
    channel_id: &str,
) -> Result<Url, ChannelMessagesReadFailure> {
    let mut url = cloud
        .graph_route(
            &["teams", team_id, "channels", channel_id, "messages"],
            None,
        )
        .map_err(|_| ChannelMessagesReadFailure::Unavailable)?;
    // The endpoint supports `$top` (maximum 50) and `$expand` only. Do not
    // add `$select`, `$orderby`, or `$expand`: root posts stay bounded and
    // replies are a separate future request contract.
    url.query_pairs_mut()
        .append_pair("$top", &CHANNEL_MESSAGE_PAGE_SIZE.to_string());
    Ok(url)
}

fn normalize_post(
    message: GraphChannelMessage,
    current_account_id: Option<&str>,
) -> Option<RemoteChannelPost> {
    let id = message.id.as_deref().and_then(normalized_identifier)?;
    let deleted = message.deleted_date_time.is_some();
    let author = sender_name(message.from.as_ref());
    let created_at = message
        .created_date_time
        .as_deref()
        .map(|value| safe_display(value, MAX_TIMESTAMP_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unknown time".into());
    let subject = message
        .subject
        .as_deref()
        .map(|value| safe_display(value, MAX_SUBJECT_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let plain_text = if deleted {
        "This channel post was deleted.".into()
    } else {
        normalize_graph_message_body(
            message
                .body
                .as_ref()
                .and_then(|body| body.content_type.as_deref()),
            message
                .body
                .as_ref()
                .and_then(|body| body.content.as_deref()),
        )
    };
    let reactions = normalize_reaction_summaries(
        &message.reactions,
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
    RemoteChannelPost::microsoft365(
        ProviderResourceRef::microsoft365(id),
        author,
        created_at,
        subject,
        plain_text,
        deleted,
        message.last_edited_date_time.is_some(),
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

fn get_collection<T: DeserializeOwned>(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphCollection<T>, ChannelMessagesReadFailure> {
    let response = get_json(session, url, cancellation)?;
    decode_collection(&response.body)
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection<T: DeserializeOwned>(
    body: &[u8],
) -> Result<GraphCollection<T>, ChannelMessagesReadFailure> {
    serde_json::from_slice::<GraphCollection<T>>(body)
        .map_err(|_| ChannelMessagesReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection::<GraphChannelMessage>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

fn get_json(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphHttpResponse, ChannelMessagesReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_CHANNEL_MESSAGES_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    check_cancelled(cancellation)?;
    validate_response_status(&response)?;
    Ok(response)
}

fn check_cancelled(
    cancellation: Option<&TransportCancellation>,
) -> Result<(), ChannelMessagesReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(ChannelMessagesReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> ChannelMessagesReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => ChannelMessagesReadFailure::Unavailable,
    }
}

fn validate_response_status(
    response: &GraphHttpResponse,
) -> Result<(), ChannelMessagesReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(ChannelMessagesReadFailure::ReauthenticationRequired),
        403 => Err(ChannelMessagesReadFailure::AccessDenied),
        429 => Err(ChannelMessagesReadFailure::Throttled(
            response.retry_after_seconds,
        )),
        _ => Err(ChannelMessagesReadFailure::Unavailable),
    }
}

fn validated_graph_next_link(
    cloud: GraphCloud,
    raw: &str,
    expected_path: &str,
) -> Result<Url, ChannelMessagesReadFailure> {
    // The opaque cursor may vary, but it must not redirect the bearer token
    // to another Graph endpoint or another selected channel.
    cloud
        .validated_next_link(raw, expected_path)
        .ok_or(ChannelMessagesReadFailure::Unavailable)
}

#[derive(Deserialize)]
struct GraphCollection<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChannelMessage {
    id: Option<String>,
    from: Option<GraphMessageFrom>,
    created_date_time: Option<String>,
    last_edited_date_time: Option<String>,
    deleted_date_time: Option<String>,
    subject: Option<String>,
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
        CHANNEL_MESSAGE_PAGE_SIZE, ChannelMessagesReadFailure, GraphHttpError, GraphHttpResponse,
        Microsoft365Transport, channel_messages_url, fetch_channel_messages,
        fetch_channel_messages_for_account, selected_channel_identifiers,
        validated_graph_next_link,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::microsoft365::GraphHttpClient;
    use crate::integrations::microsoft365::GraphSession;
    use crate::integrations::{
        AccountIdentity, ConnectionStatus, ProviderResourceRef, SyncInterest,
        TransportCancellation, WorkspaceTransport,
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
    }

    impl TestHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requested_urls: Mutex::new(Vec::new()),
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
            _max_response_bytes: u64,
        ) -> Result<GraphHttpResponse, GraphHttpError> {
            self.requested_urls
                .lock()
                .expect("test URL list must not be poisoned")
                .push(url.as_str().into());
            self.responses
                .lock()
                .expect("test response queue must not be poisoned")
                .pop_front()
                .expect("a response must be configured")
        }
    }

    struct CancellingHttpClient {
        responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
        requests: AtomicUsize,
        cancellation: TransportCancellation,
        cancel_after_request: usize,
    }

    impl CancellingHttpClient {
        fn new(
            responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>,
            cancellation: TransportCancellation,
            cancel_after_request: usize,
        ) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: AtomicUsize::new(0),
                cancellation,
                cancel_after_request,
            }
        }
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
            let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
            let response = self
                .responses
                .lock()
                .expect("test response queue must not be poisoned")
                .pop_front()
                .expect("a response must be configured");
            if request == self.cancel_after_request {
                self.cancellation.cancel();
            }
            response
        }
    }

    fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(GraphHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn selection() -> (ProviderResourceRef, ProviderResourceRef) {
        (
            ProviderResourceRef::microsoft365("team-1"),
            ProviderResourceRef::microsoft365("channel-1"),
        )
    }

    #[test]
    fn selected_channel_posts_use_only_the_documented_bounded_route() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":"post-1","from":{"user":{"displayName":" Ada\nLovelace "}},"createdDateTime":"2026-11-06T10:30:00Z","lastEditedDateTime":"2026-11-06T10:32:00Z","subject":"  Release\nupdate ","body":{"contentType":"html","content":"<p>Ready &amp; <strong>safe</strong><script>ignore()</script></p>"}}]}"#,
        )]);

        let result = fetch_channel_messages(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            None,
        )
        .expect("valid channel posts must normalize");

        assert_eq!(result.posts.len(), 1);
        let post = &result.posts[0];
        assert_eq!(post.author(), "Ada Lovelace");
        assert_eq!(post.created_at(), "2026-11-06T10:30:00Z");
        assert_eq!(post.subject(), "Release update");
        assert_eq!(post.plain_text(), "Ready & safe");
        assert!(post.is_edited());
        assert!(!post.is_deleted());
        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        let url = Url::parse(&requested[0]).expect("channel messages request must be a URL");
        assert_eq!(url.host_str(), Some("graph.microsoft.com"));
        assert_eq!(url.path(), "/v1.0/teams/team-1/channels/channel-1/messages");
        let query = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.len(), 1);
        assert_eq!(query[0].0, "$top");
        assert_eq!(query[0].1, CHANNEL_MESSAGE_PAGE_SIZE.to_string());
    }

    #[test]
    fn channel_posts_normalize_only_the_supported_reaction_palette() {
        let client = TestHttpClient::new([response(
            200,
            r#"{"value":[{"id":"post-1","reactions":[{"reactionType":"👍","user":{"user":{"id":"other-user"}}},{"reactionType":"👍","user":{"user":{"id":"current-user"}}},{"reactionType":"❤","user":{"user":{"id":"current-user"}}},{"reactionType":"😄","user":{"user":{"id":"other-user"}}},{"reactionType":"like","user":{"user":{"id":"current-user"}}},{"reactionType":"custom","user":{"user":{"id":"current-user"}}}]}]}"#,
        )]);

        let result = fetch_channel_messages_for_account(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            Some("current-user"),
            None,
        )
        .expect("reaction metadata must remain a bounded projection field");

        let post = &result.posts[0];
        assert_eq!(post.reactions().len(), 3);
        // The legacy `like` name counts with the canonical `👍`.
        assert_eq!(post.reactions()[0].kind(), ReactionKind::Like);
        assert_eq!(post.reactions()[0].count(), 3);
        assert!(post.reactions()[0].reacted_by_current_user());
        assert_eq!(post.reactions()[1].kind(), ReactionKind::Heart);
        assert_eq!(post.reactions()[1].count(), 1);
        assert!(post.reactions()[1].reacted_by_current_user());
        assert_eq!(post.reactions()[2].kind(), ReactionKind::Laugh);
        assert_eq!(post.reactions()[2].count(), 1);
        assert!(!post.reactions()[2].reacted_by_current_user());
    }

    #[test]
    fn channel_posts_follow_only_the_selected_route_and_bound_pages() {
        let client = TestHttpClient::new([
            response(
                200,
                r#"{"value":[{"id":"post-1"}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages?$skiptoken=one"}"#,
            ),
            response(
                200,
                r#"{"value":[{"id":"post-2"}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages?$skiptoken=two"}"#,
            ),
        ]);

        let result = fetch_channel_messages(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            None,
        )
        .expect("trusted pages must normalize");

        assert_eq!(result.posts.len(), 2);
        assert!(result.notices[0].contains("at most 50 root posts"));
        let requested = client
            .requested_urls
            .lock()
            .expect("test URL list must not be poisoned");
        assert_eq!(requested.len(), 2);
        assert_eq!(
            requested[1],
            "https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages?$skiptoken=one"
        );
    }

    #[test]
    fn continuations_cannot_change_the_selected_channel_route() {
        let expected = channel_messages_url(GraphCloud::Global, "team-1", "channel-1")
            .expect("channel message URL must build")
            .path()
            .to_owned();
        assert!(validated_graph_next_link(GraphCloud::Global,
            "https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages?$skiptoken=ok",
            &expected,
        )
        .is_ok());
        assert!(validated_graph_next_link(GraphCloud::Global,
            "https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-2/messages?$skiptoken=wrong",
            &expected,
        )
        .is_err());
        assert!(
            validated_graph_next_link(
                GraphCloud::Global,
                "https://example.test/v1.0/teams/team-1/channels/channel-1/messages",
                &expected,
            )
            .is_err()
        );
    }

    #[test]
    fn access_denial_replaces_the_selected_feed_without_losing_identity() {
        let client = Box::new(TestHttpClient::new([response(403, "{}")]));
        let mut transport = Microsoft365Transport::with_client("test-token", client);
        let identity = AccountIdentity {
            account_id: "u1".into(),
            display_name: "Ada".into(),
            email: None,
            tenant_id: None,
        };
        transport.identity = Some(identity.clone());
        transport.status = ConnectionStatus::Connected(identity.clone());
        let (team, channel) = selection();

        let delta = transport
            .poll(SyncInterest::ChannelMessages {
                team: team.clone(),
                channel: channel.clone(),
            })
            .expect("missing channel message scope must not disconnect identity");

        let projection = delta
            .remote_channel_messages
            .expect("an access-denied replacement is expected");
        assert!(projection.is_access_denied());
        assert_eq!(projection.team(), team);
        assert_eq!(projection.channel(), channel);
        assert!(delta.notices[0].contains("ChannelMessage.Read.All"));
        assert!(delta.notices[0].contains("membership"));
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Connected(identity) if identity.account_id == "u1"
        ));
    }

    #[test]
    fn cancellation_prevents_a_later_channel_page_request() {
        let cancellation = TransportCancellation::new();
        let client = CancellingHttpClient::new(
            [response(
                200,
                r#"{"value":[{"id":"post-1"}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/teams/team-1/channels/channel-1/messages?$skiptoken=next"}"#,
            )],
            cancellation.clone(),
            1,
        );

        let result = fetch_channel_messages(
            &GraphSession::for_test(&client),
            "team-1",
            "channel-1",
            Some(&cancellation),
        );

        assert!(matches!(result, Err(ChannelMessagesReadFailure::Cancelled)));
        assert_eq!(client.requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn local_preview_targets_are_rejected_before_networking() {
        let (team, _) = selection();
        let local_channel = ProviderResourceRef::local_preview("channel-1");

        assert!(selected_channel_identifiers(&team, &local_channel).is_err());
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

#[cfg(test)]
mod decode_collection_tests {
    use super::{ChannelMessagesReadFailure, GraphCollection, decode_collection};

    #[test]
    fn decode_collection_reads_a_page_and_its_next_link() {
        let page: GraphCollection<serde_json::Value> = decode_collection(
            br#"{"value":[{"id":"1"},{"id":"2"}],"@odata.nextLink":"https://example.invalid/next"}"#,
        )
        .expect("a well-formed page decodes");
        assert_eq!(page.value.len(), 2);
        assert_eq!(
            page.next_link.as_deref(),
            Some("https://example.invalid/next")
        );
    }

    #[test]
    fn decode_collection_maps_every_malformed_body_to_unavailable() {
        for body in [&b""[..], b"null", b"[]", b"{\"value\":3}", b"\xff\xfe"] {
            assert!(matches!(
                decode_collection::<serde_json::Value>(body),
                Err(ChannelMessagesReadFailure::Unavailable)
            ));
        }
    }
}
