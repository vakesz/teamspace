use super::super::GraphSession;
use super::{
    ChatHistoryCache, ChatReadFailure, GraphChatMessage, GraphChatViewpoint, GraphCollection,
    GraphHttpError, GraphHttpResponse, MAX_PLAIN_MESSAGE_BYTES, MAX_RAW_MESSAGE_BODY_BYTES,
    Microsoft365Transport, RemoteChatKind, SyncInterest, TransportError, chat_has_unread_preview,
    fetch_chat_pages, fetch_chat_projection, fetch_recent_messages, html_to_plain_text,
    validated_chat_messages_next_link, validated_graph_next_link,
};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::microsoft365::GraphHttpClient;
use crate::integrations::transport::AccountIdentity;
use crate::integrations::{
    ConnectionStatus, ProviderResourceRef, RemoteChatMessageImportance, TransportCancellation,
    WorkspaceTransport,
};
use crate::shared::reaction::ReactionKind;
use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use url::Url;

const CHAT_LIST_URL: &str = "https://graph.microsoft.com/v1.0/me/chats?$expand=lastMessagePreview&$orderby=lastMessagePreview/createdDateTime%20desc&$top=50";

struct TestHttpClient {
    responses: Mutex<VecDeque<Result<GraphHttpResponse, GraphHttpError>>>,
    requested_urls: Mutex<Vec<String>>,
}

impl TestHttpClient {
    fn new(responses: impl IntoIterator<Item = Result<GraphHttpResponse, GraphHttpError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requested_urls: Mutex::new(Vec::new()),
        }
    }
}

impl GraphHttpClient for TestHttpClient {
    fn get_me(&self, _url: &Url, _access_token: &str) -> Result<GraphHttpResponse, GraphHttpError> {
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
            .expect("test response list must not be poisoned")
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
    fn get_me(&self, _url: &Url, _access_token: &str) -> Result<GraphHttpResponse, GraphHttpError> {
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
            .expect("test response list must not be poisoned")
            .pop_front()
            .expect("a response must be configured");
        if request == self.cancel_after_request {
            self.cancellation.cancel();
        }
        response
    }
}

fn response(status: u16, body: &str) -> Result<GraphHttpResponse, GraphHttpError> {
    response_with_retry_after(status, body, None)
}

fn response_with_retry_after(
    status: u16,
    body: &str,
    retry_after_seconds: Option<u64>,
) -> Result<GraphHttpResponse, GraphHttpError> {
    Ok(GraphHttpResponse {
        status,
        retry_after_seconds,
        body: body.as_bytes().to_vec(),
    })
}

fn preview_at(created_date_time: Option<&str>) -> GraphChatMessage {
    GraphChatMessage {
        id: None,
        body: None,
        from: None,
        created_date_time: created_date_time.map(str::to_owned),
        deleted_date_time: None,
        last_modified_date_time: None,
        importance: None,
        mentions: Default::default(),
        reactions: Vec::new(),
        attachments: Default::default(),
        message_type: None,
        policy_violation: None,
    }
}

fn viewpoint_at(last_message_read_date_time: Option<&str>) -> GraphChatViewpoint {
    GraphChatViewpoint {
        last_message_read_date_time: last_message_read_date_time.map(str::to_owned),
    }
}

#[test]
fn unread_preview_compares_graph_datetimes_in_utc() {
    let newer_preview = preview_at(Some("2026-11-06T10:30:00Z"));
    let older_read = viewpoint_at(Some("2026-11-06T10:29:59Z"));
    assert!(chat_has_unread_preview(
        Some(&newer_preview),
        Some(&older_read)
    ));

    let equivalent_offset_read = viewpoint_at(Some("2026-11-06T11:30:00+01:00"));
    assert!(!chat_has_unread_preview(
        Some(&newer_preview),
        Some(&equivalent_offset_read)
    ));

    let later_read = viewpoint_at(Some("2026-11-06T08:31:00-02:00"));
    assert!(!chat_has_unread_preview(
        Some(&newer_preview),
        Some(&later_read)
    ));
}

#[test]
fn unread_preview_fails_safe_for_missing_or_malformed_provider_timestamps() {
    let valid_preview = preview_at(Some("2026-11-06T10:30:00.1234567Z"));
    assert!(chat_has_unread_preview(Some(&valid_preview), None));
    assert!(chat_has_unread_preview(
        Some(&valid_preview),
        Some(&viewpoint_at(Some("not-a-datetime")))
    ));
    assert!(chat_has_unread_preview(
        Some(&preview_at(None)),
        Some(&viewpoint_at(Some("2026-11-06T10:30:00Z")))
    ));
    assert!(!chat_has_unread_preview(
        None,
        Some(&viewpoint_at(Some("2026-11-06T10:30:00Z")))
    ));
}

fn chat_list(preview_id: &str) -> String {
    format!(
        r#"{{"value":[{{"id":"chat-1","lastMessagePreview":{{"id":"{preview_id}","createdDateTime":"2026-11-06T10:00:00Z","body":{{"contentType":"text","content":"Preview"}}}}}}]}}"#
    )
}

fn chat_history(text: &str) -> String {
    format!(
        r#"{{"value":[{{"id":"m2","createdDateTime":"2026-11-06T10:00:00Z","body":{{"contentType":"text","content":"{text}"}}}},{{"id":"m1","createdDateTime":"2026-11-06T09:00:00Z","body":{{"contentType":"text","content":"Earlier"}}}}]}}"#
    )
}

fn refresh(
    responses: Vec<Result<GraphHttpResponse, GraphHttpError>>,
    cache: &mut ChatHistoryCache,
    now: Instant,
) -> Vec<String> {
    let client = TestHttpClient::new(responses);
    let result = fetch_chat_projection(&GraphSession::for_test(&client), None, cache, now, None)
        .expect("the directory refresh must succeed");
    assert!(
        client
            .responses
            .lock()
            .expect("test response list must not be poisoned")
            .is_empty(),
        "every configured response must have been requested"
    );
    result.projection.conversations()[0]
        .messages()
        .iter()
        .map(|message| message.plain_text().to_owned())
        .collect()
}

#[test]
fn unchanged_preview_reuses_the_cached_history_without_requests() {
    let mut cache = ChatHistoryCache::default();
    let now = Instant::now();
    let first = refresh(
        vec![
            response(200, &chat_list("m2")),
            response(200, &chat_history("Latest")),
        ],
        &mut cache,
        now,
    );
    assert_eq!(first, ["Earlier", "Latest"]);
    // Only the list is configured: a history GET would panic.
    let second = refresh(
        vec![response(200, &chat_list("m2"))],
        &mut cache,
        now + Duration::from_secs(10),
    );
    assert_eq!(second, first);
}

#[test]
fn changed_preview_or_expired_entry_refetches_history() {
    let mut cache = ChatHistoryCache::default();
    let now = Instant::now();
    refresh(
        vec![
            response(200, &chat_list("m2")),
            response(200, &chat_history("Latest")),
        ],
        &mut cache,
        now,
    );
    let changed = refresh(
        vec![
            response(200, &chat_list("m3")),
            response(200, &chat_history("Newer")),
        ],
        &mut cache,
        now + Duration::from_secs(10),
    );
    assert_eq!(changed, ["Earlier", "Newer"]);
    let expired = refresh(
        vec![
            response(200, &chat_list("m3")),
            response(200, &chat_history("Edited")),
        ],
        &mut cache,
        now + Duration::from_secs(10) + super::CHAT_HISTORY_CACHE_TTL,
    );
    assert_eq!(expired, ["Earlier", "Edited"]);
}

#[test]
fn failed_history_is_not_cached_and_account_or_reauth_clears_entries() {
    let mut cache = ChatHistoryCache::default();
    let now = Instant::now();
    // A failed history read leaves the preview and caches nothing.
    let degraded = refresh(
        vec![response(200, &chat_list("m2")), response(500, "{}")],
        &mut cache,
        now,
    );
    assert_eq!(degraded, ["Preview"]);
    refresh(
        vec![
            response(200, &chat_list("m2")),
            response(200, &chat_history("Latest")),
        ],
        &mut cache,
        now,
    );
    // A different account never reuses the entry.
    let identity = AccountIdentity {
        account_id: "other-account".into(),
        display_name: "Other".into(),
        email: None,
        tenant_id: None,
    };
    let client = TestHttpClient::new([
        response(200, &chat_list("m2")),
        response(200, &chat_history("Latest")),
    ]);
    fetch_chat_projection(
        &GraphSession::for_test(&client),
        Some(&identity),
        &mut cache,
        now,
        None,
    )
    .expect("refresh must succeed");
    assert!(
        client
            .responses
            .lock()
            .expect("test response list must not be poisoned")
            .is_empty()
    );
    cache.clear();
    refresh(
        vec![
            response(200, &chat_list("m2")),
            response(200, &chat_history("Latest")),
        ],
        &mut cache,
        now,
    );
}

#[test]
fn cache_is_bounded_and_evicts_the_oldest_entry() {
    let mut cache = ChatHistoryCache::default();
    let now = Instant::now();
    let preview: GraphChatMessage =
        serde_json::from_str(r#"{"id":"m"}"#).expect("a minimal preview must deserialize");
    let history = super::MessageHistory {
        messages: Vec::new(),
        truncated: false,
        mentioned_user_ids: Vec::new(),
    };
    for index in 0..20_u64 {
        cache.store(
            &format!("chat-{index}"),
            Some(&preview),
            &history,
            now + Duration::from_secs(index),
        );
    }
    assert_eq!(cache.entries.len(), super::MAX_CACHED_CHAT_HISTORIES);
    assert!(!cache.entries.contains_key("chat-0"));
    assert!(cache.entries.contains_key("chat-19"));
}

#[test]
fn selected_history_reads_always_hit_graph_even_with_a_warm_cache() {
    let client = TestHttpClient::new([
        response(200, &chat_list("m2")),
        response(200, &chat_history("Latest")),
        response(200, &chat_list("m2")),
        response(200, &chat_history("Selected read")),
    ]);
    let mut transport = Microsoft365Transport::with_client("test-token", Box::new(client));
    transport.poll_chats().expect("first directory refresh");
    transport.poll_chats().expect("cached directory refresh");
    let delta = transport
        .poll_chat_messages(ProviderResourceRef::microsoft365("chat-1"))
        .expect("selected history read");
    let texts = delta
        .remote_chat_messages
        .expect("selected history projection")
        .messages()
        .iter()
        .map(|message| message.plain_text().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(texts, ["Earlier", "Selected read"]);
}

#[test]
fn history_is_ordered_by_parsed_instants_not_raw_strings() {
    let body = r#"{"value":[
        {"id":"c","createdDateTime":"2026-11-06T10:00:00.500Z","body":{"contentType":"text","content":"third"}},
        {"id":"x","createdDateTime":"not a timestamp","body":{"contentType":"text","content":"unparsable"}},
        {"id":"a","createdDateTime":"2026-11-06T10:00:00Z","body":{"contentType":"text","content":"second"}},
        {"id":"b","createdDateTime":"2026-11-06T11:30:00+02:00","body":{"contentType":"text","content":"first"}},
        {"id":"y","body":{"contentType":"text","content":"absent"}}
    ]}"#;
    let client = TestHttpClient::new([response(200, body)]);
    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("history must normalize");
    let texts = history
        .messages
        .iter()
        .map(|message| message.plain_text().to_owned())
        .collect::<Vec<_>>();
    // 11:30+02:00 is 09:30Z; `...:00Z` precedes `...:00.500Z` although the
    // raw strings compare the other way; unparsable rows keep their order.
    assert_eq!(texts, ["first", "second", "third", "unparsable", "absent"]);
}

#[test]
fn throttled_history_stops_more_history_requests_and_keeps_previews() {
    let client = TestHttpClient::new([
        response(
            200,
            r#"{"value":[{"id":"chat-1","lastMessagePreview":{"body":{"contentType":"text","content":"First preview"}}},{"id":"chat-2","lastMessagePreview":{"body":{"contentType":"text","content":"Second preview"}}},{"id":"chat-3","lastMessagePreview":{"body":{"contentType":"text","content":"Third preview"}}}]}"#,
        ),
        response_with_retry_after(429, "{}", Some(42)),
    ]);

    let result = fetch_chat_projection(
        &GraphSession::for_test(&client),
        None,
        &mut ChatHistoryCache::default(),
        Instant::now(),
        None,
    )
    .expect("a nested history throttle must retain the chat list");
    let conversations = result.projection.conversations();
    assert_eq!(conversations.len(), 3);
    assert_eq!(conversations[0].messages()[0].plain_text(), "First preview");
    assert_eq!(
        conversations[1].messages()[0].plain_text(),
        "Second preview"
    );
    assert_eq!(conversations[2].messages()[0].plain_text(), "Third preview");
    assert!(result.notices.iter().any(|notice| {
        notice == "Microsoft 365 chat history sync was throttled; retry after 42 seconds."
    }));
    assert!(
        result
            .notices
            .iter()
            .any(|notice| notice.contains("last-message previews remain available"))
    );

    let requested = client
        .requested_urls
        .lock()
        .expect("test URL list must not be poisoned");
    assert_eq!(requested.len(), 2, "no later chat history is requested");
    assert!(requested[1].contains("/chats/chat-1/messages"));
}

#[test]
fn recent_messages_render_in_created_datetime_order() {
    let client = TestHttpClient::new([response(
        200,
        r#"{"value":[{"body":{"contentType":"text","content":"Older message, edited later"},"createdDateTime":"2026-11-05T12:00:00Z","lastModifiedDateTime":"2026-11-06T14:00:00Z"},{"body":{"contentType":"text","content":"Newer message"},"createdDateTime":"2026-11-06T11:00:00Z","lastModifiedDateTime":"2026-11-06T13:00:00Z"}]}"#,
    )]);

    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("message history must normalize");
    let rendered = history
        .messages
        .iter()
        .map(|message| message.plain_text())
        .collect::<Vec<_>>();
    assert_eq!(rendered, ["Older message, edited later", "Newer message"]);

    let requested = client
        .requested_urls
        .lock()
        .expect("test URL list must not be poisoned");
    let url = Url::parse(&requested[0]).expect("message route must be a URL");
    assert!(
        url.query_pairs()
            .any(|(key, value)| { key == "$orderby" && value == "createdDateTime desc" })
    );
}

#[test]
fn malformed_reactions_never_discard_an_otherwise_valid_message() {
    let client = TestHttpClient::new([response(
        200,
        r#"{"value":[{"id":"m1","messageType":"message","body":{"contentType":"text","content":"Null"},"createdDateTime":"2026-11-06T11:00:00Z","reactions":null},{"id":"m2","body":{"contentType":"text","content":"Mixed"},"createdDateTime":"2026-11-06T12:00:00Z","reactions":[7,{"reactionType":5},{"reactionType":"like","user":{"user":{"id":"u2"}}}]}]}"#,
    )]);

    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("optional reaction metadata must not poison history");

    assert_eq!(history.messages.len(), 2);
    assert!(history.messages[0].reactions().is_empty());
    assert_eq!(history.messages[1].reactions().len(), 1);
    assert_eq!(
        history.messages[1].reactions()[0].kind(),
        ReactionKind::Like
    );
}

#[test]
fn chat_message_reactions_normalize_only_the_supported_palette_and_optional_targets() {
    let client = TestHttpClient::new([response(
        200,
        r#"{"value":[{"id":"message-with-reaction","body":{"contentType":"text","content":"Reactable"},"createdDateTime":"2026-11-06T11:00:00Z","reactions":[{"reactionType":"👍","user":{"user":{"id":"u1"}}},{"reactionType":"👍","user":{"user":{"id":"u2"}}},{"reactionType":"❤","user":{"user":{"id":"u1"}}},{"reactionType":"😢","user":{"user":{"id":"u2"}}},{"reactionType":"❤️","user":{"user":{"id":"u1"}}},{"reactionType":"custom","user":{"user":{"id":"u1"}}}]},{"body":{"contentType":"text","content":"Display only"},"createdDateTime":"2026-11-06T12:00:00Z","reactions":[{"reactionType":"👍","user":{"user":{"id":"u2"}}}]}]}"#,
    )]);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    };

    let history = fetch_recent_messages(
        &GraphSession::for_test(&client),
        "chat-1",
        Some(&identity),
        None,
    )
    .expect("message history must normalize");

    assert_eq!(history.messages.len(), 2);
    let reactable = &history.messages[0];
    assert_eq!(
        reactable
            .target()
            .and_then(|target| target.microsoft365_id().map(str::to_owned)),
        Some("message-with-reaction".into())
    );
    assert_eq!(reactable.reactions().len(), 3);
    assert_eq!(reactable.reactions()[0].kind(), ReactionKind::Like);
    assert_eq!(reactable.reactions()[0].count(), 2);
    assert!(reactable.reactions()[0].reacted_by_current_user());
    // `❤` and the variation-selector form `❤️` are one Heart kind.
    assert_eq!(reactable.reactions()[1].kind(), ReactionKind::Heart);
    assert_eq!(reactable.reactions()[1].count(), 2);
    assert!(reactable.reactions()[1].reacted_by_current_user());
    assert_eq!(reactable.reactions()[2].kind(), ReactionKind::Sad);
    assert_eq!(reactable.reactions()[2].count(), 1);
    assert!(!reactable.reactions()[2].reacted_by_current_user());

    // A malformed/missing target stays presentation-only even when Graph
    // included a reaction collection; it cannot become a generic route.
    let display_only = &history.messages[1];
    assert!(display_only.target().is_none());
    assert_eq!(display_only.reactions().len(), 1);
    assert_eq!(display_only.reactions()[0].kind(), ReactionKind::Like);
    assert_eq!(display_only.reactions()[0].count(), 1);
    assert!(!display_only.reactions()[0].reacted_by_current_user());
}

#[test]
fn chat_message_importance_accepts_only_exact_graph_values_and_fails_safe() {
    let response_body = r#"{"value":[
            {"body":{"contentType":"text","content":"Normal"},"createdDateTime":"2026-11-06T11:00:00Z","importance":"normal"},
            {"body":{"contentType":"text","content":"High"},"createdDateTime":"2026-11-06T11:01:00Z","importance":"high"},
            {"body":{"contentType":"text","content":"Urgent"},"createdDateTime":"2026-11-06T11:02:00Z","importance":"urgent"},
            {"body":{"contentType":"text","content":"Case variant"},"createdDateTime":"2026-11-06T11:03:00Z","importance":"HIGH"},
            {"body":{"contentType":"text","content":"Unknown"},"createdDateTime":"2026-11-06T11:04:00Z","importance":"custom"},
            {"body":{"contentType":"text","content":"Null"},"createdDateTime":"2026-11-06T11:05:00Z","importance":null},
            {"body":{"contentType":"text","content":"Number"},"createdDateTime":"2026-11-06T11:06:00Z","importance":7},
            {"body":{"contentType":"text","content":"Object"},"createdDateTime":"2026-11-06T11:07:00Z","importance":{}},
            {"body":{"contentType":"text","content":"Absent"},"createdDateTime":"2026-11-06T11:08:00Z"}
        ]}"#;
    let parsed: GraphCollection<GraphChatMessage> = serde_json::from_str(response_body)
        .expect("the priority fixture must match Graph's optional shape");
    assert_eq!(parsed.value.len(), 9);
    let client = TestHttpClient::new([response(200, response_body)]);

    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("malformed optional priority must not reject safe history rows");

    assert_eq!(
        history
            .messages
            .iter()
            .map(|message| message.importance())
            .collect::<Vec<_>>(),
        [
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::High,
            RemoteChatMessageImportance::Urgent,
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::Normal,
            RemoteChatMessageImportance::Normal,
        ]
    );
}

#[test]
fn message_reference_attachment_normalizes_one_bounded_safe_quote_context() {
    let reference = serde_json::json!({
        "messageId": "quoted-message-id",
        "messagePreview": "<b>Review</b> this <script>omit</script> proposal",
        "messageSender": {"user": {"displayName": "Ada Lovelace"}}
    })
    .to_string();
    let body = serde_json::json!({"value": [{
        "id": "quote-reply-id",
        "body": {"contentType": "text", "content": "I agree"},
        "createdDateTime": "2026-11-06T12:00:00Z",
        "attachments": [{"contentType": "messageReference", "content": reference}]
    }]})
    .to_string();
    let client = TestHttpClient::new([response(200, &body)]);

    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("a valid message reference must not reject history");
    let quote = history.messages[0]
        .quotes()
        .first()
        .expect("one valid messageReference must retain a quote context");
    assert_eq!(quote.sender_name(), "Ada Lovelace");
    assert_eq!(quote.preview(), "Review this proposal");
    assert_eq!(quote.target().microsoft365_id(), Some("quoted-message-id"));
    assert!(!format!("{quote:?}").contains("quoted-message-id"));
}

#[test]
fn malformed_duplicate_or_unbounded_message_references_fail_closed() {
    let valid_reference = serde_json::json!({
        "messageId": "quoted-message-id",
        "messagePreview": "A safe preview",
        "messageSender": {"user": {"displayName": "Ada"}}
    })
    .to_string();
    let oversized_reference = "x".repeat(super::MAX_MESSAGE_REFERENCE_CONTENT_BYTES + 1);
    let body = serde_json::json!({"value": [
        {"body": {"contentType": "text", "content": "Malformed"}, "attachments": [{"contentType": "messageReference", "content": "not JSON"}]},
        {"body": {"contentType": "text", "content": "Duplicate"}, "attachments": [
            {"contentType": "messageReference", "content": valid_reference.clone()},
            {"contentType": "messageReference", "content": valid_reference.clone()}
        ]},
        {"body": {"contentType": "text", "content": "Null"}, "attachments": null},
        {"body": {"contentType": "text", "content": "Wrong"}, "attachments": {"contentType": "messageReference"}},
        {"body": {"contentType": "text", "content": "Oversized"}, "attachments": [{"contentType": "messageReference", "content": oversized_reference}]}
    ]})
    .to_string();
    let client = TestHttpClient::new([response(200, &body)]);

    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("optional malformed attachments must not reject history");
    assert_eq!(history.messages.len(), 5);
    assert!(
        history
            .messages
            .iter()
            .all(|message| message.quotes().is_empty())
    );
}

fn reference_attachment(id: &str) -> serde_json::Value {
    serde_json::json!({
        "contentType": "messageReference",
        "content": serde_json::json!({
            "messageId": id,
            "messagePreview": "preview",
            "messageSender": {"user": {"displayName": "Ada"}}
        })
        .to_string()
    })
}

fn quotes_of_one_message(attachments: Vec<serde_json::Value>) -> Vec<String> {
    let body = serde_json::json!({"value": [{
        "id": "reply",
        "body": {"contentType": "text", "content": "Reply"},
        "attachments": attachments
    }]})
    .to_string();
    let client = TestHttpClient::new([response(200, &body)]);
    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("history must load");
    history.messages[0]
        .quotes()
        .iter()
        .map(|quote| {
            quote
                .target()
                .microsoft365_id()
                .expect("normalized quote target")
                .to_owned()
        })
        .collect()
}

#[test]
fn message_references_normalize_zero_one_and_ten_in_attachment_order() {
    assert!(quotes_of_one_message(Vec::new()).is_empty());
    assert_eq!(
        quotes_of_one_message(vec![reference_attachment("m0")]),
        ["m0"]
    );
    let ten: Vec<_> = (0..10)
        .map(|n| reference_attachment(&format!("m{n}")))
        .collect();
    let ids = quotes_of_one_message(ten);
    assert_eq!(ids, (0..10).map(|n| format!("m{n}")).collect::<Vec<_>>());
}

#[test]
fn eleven_references_and_the_sixteen_attachment_bound_fail_closed() {
    let eleven: Vec<_> = (0..11)
        .map(|n| reference_attachment(&format!("m{n}")))
        .collect();
    assert!(quotes_of_one_message(eleven).is_empty());
    // Ten references plus six unrelated attachments is exactly the bound.
    let mut sixteen: Vec<_> = (0..10)
        .map(|n| reference_attachment(&format!("m{n}")))
        .collect();
    sixteen.extend((0..6).map(|_| serde_json::json!({"contentType": "file"})));
    assert_eq!(quotes_of_one_message(sixteen).len(), 10);
    let mut seventeen: Vec<_> = (0..10)
        .map(|n| reference_attachment(&format!("m{n}")))
        .collect();
    seventeen.extend((0..7).map(|_| serde_json::json!({"contentType": "file"})));
    assert!(quotes_of_one_message(seventeen).is_empty());
}

#[test]
fn one_duplicate_or_malformed_reference_drops_the_whole_set() {
    assert!(
        quotes_of_one_message(vec![
            reference_attachment("a"),
            reference_attachment("b"),
            reference_attachment("a"),
        ])
        .is_empty()
    );
    assert!(
        quotes_of_one_message(vec![
            reference_attachment("a"),
            serde_json::json!({"contentType": "messageReference", "content": "not JSON"}),
            reference_attachment("b"),
        ])
        .is_empty()
    );
}

#[test]
fn chat_mentions_match_only_the_verified_account_identifier() {
    let client = TestHttpClient::new([response(
        200,
        r#"{"value":[
            {"body":{"contentType":"html","content":"<at id=\"0\">Ada Lovelace</at> please review"},"createdDateTime":"2026-11-06T11:00:00Z","mentions":[{"id":0,"mentionText":"Ada Lovelace","mentioned":{"user":{"id":"u1","displayName":"Ada Lovelace"}}}]},
            {"body":{"contentType":"html","content":"<at id=\"0\">Ada Lovelace</at> is someone else"},"createdDateTime":"2026-11-06T11:01:00Z","mentions":[{"id":0,"mentionText":"Ada Lovelace","mentioned":{"user":{"id":"u2","displayName":"Ada Lovelace"}}}]},
            {"body":{"contentType":"text","content":"Malformed mention metadata stays display-only"},"createdDateTime":"2026-11-06T11:02:00Z","mentions":[{}, {"mentioned":{}}, {"mentioned":{"user":{"id":"u1 "}}}, {"mentioned":{"application":{"id":"u1"}}}, {"mentioned":{"user":"not-an-identity"}}]},
            {"body":{"contentType":"text","content":"No mention collection"},"createdDateTime":"2026-11-06T11:03:00Z"},
            {"body":{"contentType":"text","content":"Null mention collection"},"createdDateTime":"2026-11-06T11:04:00Z","mentions":null},
            {"body":{"contentType":"text","content":"Wrong-shape mention collection"},"createdDateTime":"2026-11-06T11:05:00Z","mentions":{"not":"an array"}}
        ]}"#,
    )]);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada Lovelace".into(),
        email: None,
        tenant_id: None,
    };

    let history = fetch_recent_messages(
        &GraphSession::for_test(&client),
        "chat-1",
        Some(&identity),
        None,
    )
    .expect("nonessential malformed mention entries must not reject the history");

    assert_eq!(history.messages.len(), 6);
    assert!(history.messages[0].mentions_current_user());
    assert!(history.messages[0].plain_text().contains("Ada Lovelace"));
    assert!(
        !history.messages[1].mentions_current_user(),
        "a matching display name with another provider ID is not a current-user mention"
    );
    assert!(
        !history.messages[2].mentions_current_user(),
        "missing, unrelated, and nonexact identity values fail closed"
    );
    assert!(
        !history.messages[3].mentions_current_user(),
        "an absent mention collection is not a current-user mention"
    );
    assert!(
        !history.messages[4].mentions_current_user(),
        "a null mention collection is not a current-user mention"
    );
    assert!(
        !history.messages[5].mentions_current_user(),
        "a wrong-shape mention collection is discarded without losing the history"
    );
}

#[test]
fn selected_chat_history_is_target_bound_and_chronological() {
    let client = Box::new(TestHttpClient::new([response(
        200,
        r#"{"value":[{"body":{"contentType":"text","content":"Newest"},"from":{"user":{"id":"u1","displayName":"Ada Lovelace"}},"createdDateTime":"2026-11-06T12:00:00Z"},{"body":{"contentType":"html","content":"<p>Earlier</p>"},"from":{"user":{"id":"u2","displayName":"Grace Hopper"}},"createdDateTime":"2026-11-06T11:00:00Z"}]}"#,
    )]));
    let mut transport = Microsoft365Transport::with_client("test-token", client);
    let chat = ProviderResourceRef::microsoft365("19:chat@thread.v2");

    let delta = transport
        .poll(SyncInterest::ChatMessages { chat: chat.clone() })
        .expect("selected history must normalize");

    assert_eq!(
        delta.completed,
        [SyncInterest::ChatMessages { chat: chat.clone() }]
    );
    assert!(delta.remote_chats.is_none());
    let projection = delta
        .remote_chat_messages
        .expect("selected-history projection must be present");
    assert_eq!(projection.chat(), chat);
    assert!(!projection.is_access_denied());
    assert_eq!(projection.messages().len(), 2);
    assert_eq!(projection.messages()[0].plain_text(), "Earlier");
    assert_eq!(projection.messages()[1].plain_text(), "Newest");
}

#[test]
fn selected_chat_history_access_denied_preserves_identity_and_target() {
    let client = Box::new(TestHttpClient::new([response(403, "{}")])) as Box<dyn GraphHttpClient>;
    let mut transport = Microsoft365Transport::with_client("test-token", client);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    };
    transport.identity = Some(identity.clone());
    transport.status = ConnectionStatus::Connected(identity);
    let chat = ProviderResourceRef::microsoft365("19:chat@thread.v2");

    let delta = transport
        .poll(SyncInterest::ChatMessages { chat: chat.clone() })
        .expect("a selected history denial must be a scoped projection");

    assert!(delta.remote_chats.is_none());
    let projection = delta
        .remote_chat_messages
        .expect("access-denied selected-history projection must be present");
    assert_eq!(projection.chat(), chat);
    assert!(projection.is_access_denied());
    assert!(projection.messages().is_empty());
    assert!(
        delta
            .notices
            .iter()
            .any(|notice| notice.contains("Chat.Read"))
    );
    assert!(matches!(
        transport.connection_status(),
        ConnectionStatus::Connected(identity) if identity.account_id == "u1"
    ));
}

#[test]
fn chat_history_continuations_cannot_change_the_selected_chat_route() {
    let expected_path = "/v1.0/chats/19%3Achat%40thread.v2/messages";
    assert!(validated_chat_messages_next_link(GraphCloud::Global,
        "https://graph.microsoft.com/v1.0/chats/19%3Achat%40thread.v2/messages?$skiptoken=opaque",
        expected_path,
    )
    .is_ok());
    assert!(
        validated_chat_messages_next_link(
            GraphCloud::Global,
            "https://graph.microsoft.com/v1.0/chats/other/messages?$skiptoken=wrong-chat",
            expected_path,
        )
        .is_err()
    );
    assert!(
        validated_chat_messages_next_link(
            GraphCloud::Global,
            "https://graph.microsoft.com/v1.0/me/messages?$skiptoken=wrong-resource",
            expected_path,
        )
        .is_err()
    );
    assert!(
        validated_chat_messages_next_link(
            GraphCloud::Global,
            "https://example.test/v1.0/chats/19%3Achat%40thread.v2/messages?$skiptoken=wrong-host",
            expected_path,
        )
        .is_err()
    );
}

#[test]
fn cancellation_stops_selected_chat_history_before_a_follow_up_page() {
    let cancellation = TransportCancellation::new();
    let client = Box::new(CancellingHttpClient::new(
        [response(
            200,
            r#"{"value":[],"@odata.nextLink":"https://graph.microsoft.com/v1.0/chats/19%3Achat%40thread.v2/messages?$skiptoken=opaque"}"#,
        )],
        cancellation.clone(),
        1,
    ));
    let mut transport = Microsoft365Transport::with_client("test-token", client);

    let result = transport.poll_cancellable(
        SyncInterest::ChatMessages {
            chat: ProviderResourceRef::microsoft365("19:chat@thread.v2"),
        },
        &cancellation,
    );

    assert_eq!(result, Err(TransportError::Cancelled));
}

#[test]
fn chat_poll_normalizes_safe_rows_and_recent_messages() {
    let client = Box::new(TestHttpClient::new([
        response(
            200,
            r#"{"value":[{"id":"19:chat@thread.v2","chatType":"oneOnOne","lastMessagePreview":{"body":{"contentType":"html","content":"<div>Hello &amp; <at id=\"0\">Ada</at></div>"},"from":{"user":{"id":"u1","displayName":"Ada Lovelace"}},"createdDateTime":"2026-11-06T12:00:00Z"}}]}"#,
        ),
        response(
            200,
            r#"{"value":[{"id":"latest-message","body":{"contentType":"html","content":"<p>Latest <strong>message</strong></p>"},"from":{"user":{"id":"u1","displayName":"Ada Lovelace"}},"createdDateTime":"2026-11-06T12:00:00Z","reactions":[{"reactionType":"👍","user":{"user":{"id":"u1"}}}]},{"id":"earlier-message","body":{"contentType":"text","content":"Earlier"},"from":{"user":{"id":"u2","displayName":"Grace Hopper"}},"createdDateTime":"2026-11-06T11:00:00Z"}]}"#,
        ),
    ]));
    let mut transport = Microsoft365Transport::with_client("test-token", client);
    transport.identity = Some(AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada Lovelace".into(),
        email: None,
        tenant_id: None,
    });
    transport.status = ConnectionStatus::Connected(
        transport
            .identity
            .clone()
            .expect("test identity must be present"),
    );

    let delta = transport
        .poll(SyncInterest::Chats)
        .expect("chat read must normalize");
    assert_eq!(delta.completed, [SyncInterest::Chats]);
    let projection = delta.remote_chats.expect("chat projection must be present");
    assert_eq!(projection.conversations().len(), 1);
    let chat = &projection.conversations()[0];
    assert_eq!(chat.kind(), RemoteChatKind::OneOnOne);
    assert_eq!(chat.title(), "One-to-one chat");
    assert_eq!(chat.messages().len(), 2);
    assert_eq!(chat.messages()[1].plain_text(), "Latest message");
    assert!(chat.messages()[1].sent_by_current_user());
    assert_eq!(
        chat.messages()[1]
            .target()
            .and_then(|target| target.microsoft365_id().map(str::to_owned)),
        Some("latest-message".into())
    );
    assert_eq!(chat.messages()[1].reactions().len(), 1);
    assert_eq!(chat.messages()[1].reactions()[0].kind(), ReactionKind::Like);
    assert_eq!(chat.messages()[1].reactions()[0].count(), 1);
    assert!(chat.messages()[1].reactions()[0].reacted_by_current_user());
}

#[test]
fn chat_scope_denial_preserves_verified_identity_and_returns_safe_notice() {
    let client = Box::new(TestHttpClient::new([response(403, "{}")])) as Box<dyn GraphHttpClient>;
    let mut transport = Microsoft365Transport::with_client("test-token", client);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    };
    transport.identity = Some(identity.clone());
    transport.status = ConnectionStatus::Connected(identity);

    let delta = transport
        .poll(SyncInterest::Chats)
        .expect("a missing Chat.Read scope must not break identity");

    assert!(
        delta
            .remote_chats
            .expect("empty replacement expected")
            .is_empty()
    );
    assert!(delta.notices[0].contains("Chat.Read"));
    assert!(matches!(
        transport.connection_status(),
        ConnectionStatus::Connected(identity) if identity.account_id == "u1"
    ));
}

#[test]
fn untrusted_paging_links_are_rejected_without_becoming_requests() {
    let path = "/v1.0/me/chats";
    for link in [
        "https://example.test/v1.0/me/chats",
        "http://graph.microsoft.com/v1.0/me/chats",
        "https://graph.microsoft.com/beta/me/chats",
        "https://graph.microsoft.com/v1.0/me/messages",
        "https://graph.microsoft.com/v1.0/users/x/chats",
    ] {
        assert!(
            validated_graph_next_link(GraphCloud::Global, link, path).is_err(),
            "{link}"
        );
    }
    assert!(
        validated_graph_next_link(
            GraphCloud::Global,
            "https://graph.microsoft.com/v1.0/me/chats",
            path
        )
        .is_ok()
    );
    assert!(Url::parse(CHAT_LIST_URL).is_ok());
}

#[test]
fn chat_list_follows_only_the_trusted_opaque_next_link() {
    let client = TestHttpClient::new([
        response(
            200,
            r#"{"value":[{"id":"chat-1"}],"@odata.nextLink":"https://graph.microsoft.com/v1.0/me/chats?$skiptoken=opaque"}"#,
        ),
        response(200, r#"{"value":[{"id":"chat-2"}]}"#),
    ]);

    let (chats, truncated) = fetch_chat_pages(&GraphSession::for_test(&client), None)
        .expect("trusted Graph continuation must be followed");

    assert_eq!(chats.len(), 2);
    assert!(!truncated);
    let requested = client
        .requested_urls
        .lock()
        .expect("test URL list must not be poisoned");
    assert_eq!(requested.len(), 2);
    assert_eq!(requested[0], CHAT_LIST_URL);
    assert_eq!(
        requested[1],
        "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=opaque"
    );
}

#[test]
fn cancellation_stops_chat_sync_between_bounded_graph_requests() {
    let cancellation = TransportCancellation::new();
    let client = CancellingHttpClient::new(
        [
            response(200, r#"{"value":[{"id":"chat-1"},{"id":"chat-2"}]}"#),
            response(200, r#"{"value":[]}"#),
        ],
        cancellation.clone(),
        2,
    );

    let result = fetch_chat_projection(
        &GraphSession::for_test(&client),
        None,
        &mut ChatHistoryCache::default(),
        Instant::now(),
        Some(&cancellation),
    );

    assert!(matches!(result, Err(ChatReadFailure::Cancelled)));
    assert_eq!(
        client.requests.load(Ordering::Relaxed),
        2,
        "a cancelled history request must prevent later histories or pages"
    );
}

#[test]
fn html_is_rendered_as_safe_plain_text() {
    assert_eq!(
        html_to_plain_text(
            "<div>Hello <strong>world</strong> &amp; <at>Ada</at><script>alert(1)</script></div>"
        ),
        "Hello world & Ada"
    );
    assert_eq!(
        html_to_plain_text("<style>body{display:none}</style>Visible"),
        "Visible"
    );
}

#[test]
fn malformed_html_delimiters_have_bounded_lookahead() {
    let malformed_tags = "<".repeat(MAX_RAW_MESSAGE_BODY_BYTES);
    assert_eq!(html_to_plain_text(&malformed_tags), "");

    let malformed_entities = "&".repeat(MAX_RAW_MESSAGE_BODY_BYTES);
    let rendered = html_to_plain_text(&malformed_entities);
    assert_eq!(rendered.len(), MAX_PLAIN_MESSAGE_BYTES);
    assert!(rendered.bytes().all(|byte| byte == b'&'));
}

#[test]
fn unavailable_chat_read_preserves_identity_status() {
    let client =
        Box::new(TestHttpClient::new([Err(GraphHttpError::Network)])) as Box<dyn GraphHttpClient>;
    let mut transport = Microsoft365Transport::with_client("test-token", client);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    };
    transport.identity = Some(identity.clone());
    transport.status = ConnectionStatus::Connected(identity);

    assert_eq!(
        transport.poll(SyncInterest::Chats),
        Err(TransportError::Unavailable(
            "Microsoft Graph chat synchronization is temporarily unavailable".into()
        ))
    );
    assert!(matches!(
        transport.connection_status(),
        ConnectionStatus::Connected(identity) if identity.account_id == "u1"
    ));
}

#[test]
fn chat_list_history_routes_and_next_links_follow_the_cloud() {
    for cloud in GraphCloud::ALL {
        let client = TestHttpClient::new([response(200, r#"{"value":[]}"#)]);
        fetch_chat_pages(&GraphSession::for_test_in(cloud, &client), None)
            .unwrap_or_else(|_| panic!("chat list must read"));
        let requested = client.requested_urls.lock().expect("lock");
        let url = Url::parse(&requested[0]).expect("URL");
        assert_eq!(url.host_str(), Some(cloud.graph_host()));
        assert_eq!(url.path(), "/v1.0/me/chats");
        drop(requested);

        let history = super::chat_messages_url(cloud, "chat-1")
            .unwrap_or_else(|_| panic!("history route must build"));
        assert_eq!(history.host_str(), Some(cloud.graph_host()));
        let path = history.path().to_owned();
        let own = format!("https://{}{path}?$skiptoken=x", cloud.graph_host());
        assert!(validated_chat_messages_next_link(cloud, &own, &path).is_ok());
        assert!(validated_graph_next_link(cloud, &own, &path).is_ok());
        for other in GraphCloud::ALL.into_iter().filter(|other| *other != cloud) {
            let cross = format!("https://{}{path}?$skiptoken=x", other.graph_host());
            assert!(validated_chat_messages_next_link(cloud, &cross, &path).is_err());
            assert!(validated_graph_next_link(cloud, &cross, &path).is_err());
        }
    }
}

#[test]
fn formatted_html_bodies_gain_rich_runs_while_plain_text_stays_authoritative() {
    let client = TestHttpClient::new([response(
        200,
        r#"{"value":[
            {"id":"m1","messageType":"message","body":{"contentType":"html","content":"<p>see <strong>this</strong> <a href=\"https://example.com/x\">link</a></p>"},"createdDateTime":"2026-11-06T11:00:00Z","from":{"user":{"id":"u1","displayName":"Ada"}}},
            {"id":"m2","body":{"contentType":"html","content":"<p>just text</p>"},"createdDateTime":"2026-11-06T11:01:00Z"},
            {"id":"m3","body":{"contentType":"text","content":"**not bold**"},"createdDateTime":"2026-11-06T11:02:00Z"},
            {"id":"m4","body":{"contentType":"html","content":"<b>x</b><a href=\"javascript:alert(1)\">bad</a>"},"createdDateTime":"2026-11-06T11:03:00Z"},
            {"id":"m5","body":{"contentType":"html","content":"<b>unclosed"},"createdDateTime":"2026-11-06T11:04:00Z"}
        ]}"#,
    )]);
    let identity = AccountIdentity {
        account_id: "u1".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    };
    let history = fetch_recent_messages(
        &GraphSession::for_test(&client),
        "chat-1",
        Some(&identity),
        None,
    )
    .expect("history reads");
    let messages = &history.messages;
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0].plain_text(), "see this link");
    assert!(messages[0].rich().is_some_and(|runs| runs.len() == 4));
    assert!(messages[0].is_plain_text_editable());
    assert!(messages[1].rich().is_none());
    assert!(messages[2].rich().is_none());
    // A non-https link is never a link run, and a malformed body has no runs.
    assert!(messages[3].rich().is_some_and(|runs| {
        runs.iter()
            .all(|run| matches!(run, crate::integrations::Inline::Text { .. }))
    }));
    assert!(messages[4].rich().is_none());
    assert_eq!(messages[4].plain_text(), "unclosed");
    assert!(!messages[4].is_plain_text_editable());
}

fn file_reference(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "contentType": "reference",
        "contentUrl": "https://contoso.sharepoint.com/SECRET-FILE-URL/Budget.docx",
        "name": name,
        "thumbnailUrl": "https://contoso.sharepoint.com/SECRET-THUMBNAIL"
    })
}

fn file_references_of_one_message(
    attachments: Vec<serde_json::Value>,
) -> crate::integrations::RemoteChatMessage {
    let body = serde_json::json!({"value": [{
        "id": "file-message",
        "messageType": "message",
        "from": {"user": {"id": "me-id", "displayName": "Me"}},
        "body": {"contentType": "html", "content": "<p>Budget</p><attachment id=\"x\"></attachment>"},
        "attachments": attachments
    }]})
    .to_string();
    let client = TestHttpClient::new([response(200, &body)]);
    let history = fetch_recent_messages(&GraphSession::for_test(&client), "chat-1", None, None)
        .expect("history must load");
    history.messages.into_iter().next().expect("one message")
}

const FILE_GUID: &str = "153fa47d-18c9-4179-be08-9879815a9f90";

#[test]
fn a_file_reference_becomes_a_name_and_a_guid_and_never_a_url() {
    let message = file_references_of_one_message(vec![file_reference(FILE_GUID, "Budget.docx")]);
    let [attachment] = message.attachments() else {
        panic!("one attachment");
    };
    assert_eq!(attachment.name(), "Budget.docx");
    assert_eq!(attachment.id(), FILE_GUID);
    let debug = format!("{message:?}");
    for leaked in [
        "SECRET-FILE-URL",
        "SECRET-THUMBNAIL",
        FILE_GUID,
        "Budget.docx",
    ] {
        assert!(!debug.contains(leaked), "{leaked} leaked into Debug");
    }
    assert_eq!(message.plain_text(), "Budget");
}

#[test]
fn file_references_are_lenient_and_bounded() {
    // An upper-case GUID is normalized; a bad GUID, a nameless reference, a
    // non-reference attachment and a messageReference are skipped.
    let message = file_references_of_one_message(vec![
        file_reference(&FILE_GUID.to_ascii_uppercase(), "Upper.docx"),
        file_reference("not-a-guid", "BadId.docx"),
        file_reference(FILE_GUID, "   "),
        serde_json::json!({"id": FILE_GUID, "contentType": "file", "name": "Other.docx"}),
        reference_attachment("m0"),
    ]);
    assert_eq!(
        message
            .attachments()
            .iter()
            .map(|attachment| (attachment.id(), attachment.name()))
            .collect::<Vec<_>>(),
        [(FILE_GUID, "Upper.docx")]
    );
    assert_eq!(
        message.quotes().len(),
        1,
        "a quote is still read beside files"
    );

    let ten_plus: Vec<_> = (0..12)
        .map(|n| file_reference(FILE_GUID, &format!("f{n}.txt")))
        .collect();
    assert_eq!(
        file_references_of_one_message(ten_plus).attachments().len(),
        10
    );
}

#[test]
fn a_hostile_file_name_is_made_inert_and_bounded() {
    let hostile = format!("evil\u{202E}\u{200B}exe.txt{}", "x".repeat(400));
    let message = file_references_of_one_message(vec![file_reference(FILE_GUID, &hostile)]);
    let name = message.attachments()[0].name();
    assert!(!name.contains('\u{202E}') && !name.contains('\u{200B}'));
    assert!(name.len() <= 128);
}

#[test]
fn a_message_that_carries_a_file_is_never_editable() {
    let message = file_references_of_one_message(vec![file_reference(FILE_GUID, "Budget.docx")]);
    assert!(!message.is_plain_text_editable());
}
