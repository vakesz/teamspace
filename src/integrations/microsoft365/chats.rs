//! Microsoft Graph chat read normalization.
//!
//! This module owns the Graph route shapes, opaque continuation links,
//! response limits, DTOs, and HTML-to-plain-text conversion. It returns only
//! provider-normalized projections to the transport seam. This read module is
//! deliberately separate from `messages` and `chat_reactions`: `Chat.Read`
//! is required for message content, while plaintext sends and bounded typed
//! reaction actions have separate explicit mutation routes.

use super::GraphSession;
use super::reactions::{deserialize_reactions, normalize_reaction_summaries};
use super::text::{self, safe_display, truncate_utf8};
use super::{GraphHttpError, GraphHttpResponse, Microsoft365Transport};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::transport::{
    AccountIdentity, MAX_REMOTE_CHAT_ATTACHMENTS, MAX_REMOTE_QUOTE_SOURCES, ProviderResourceRef,
    RemoteChatAttachment, RemoteChatConversation, RemoteChatKind, RemoteChatMessage,
    RemoteChatMessageImportance, RemoteChatMessageQuote, RemoteChatMessagesProjection,
    RemoteChatProjection, RichRun,
};
use crate::integrations::{SyncInterest, TransportCancellation, TransportError, WorkspaceDelta};
use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use url::Url;

/// Raw query so the wire form (including the `%20` in `$orderby`) is stable.
const GRAPH_CHAT_LIST_QUERY: &str =
    "$expand=lastMessagePreview&$orderby=lastMessagePreview/createdDateTime%20desc&$top=50";
const MAX_CHAT_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_CHAT_PAGES: usize = 4;
const MAX_REMOTE_CHATS: usize = 100;
const MAX_MESSAGE_PAGES_PER_CHAT: usize = 2;
const MAX_MESSAGES_PER_CHAT: usize = 100;
const MAX_DETAILED_CHAT_HISTORIES: usize = 4;
/// A directory refresh may reuse a chat's cached history only while its
/// last-message preview is unchanged and the entry is younger than this. The
/// preview does not change for a reaction or an edit to an older message, so
/// the age bound keeps those from staying stale indefinitely.
const CHAT_HISTORY_CACHE_TTL: Duration = Duration::from_secs(120);
const MAX_CACHED_CHAT_HISTORIES: usize = 16;
const MAX_CHAT_TITLE_BYTES: usize = 256;
const MAX_CHAT_SUBTITLE_BYTES: usize = 512;
const MAX_SENDER_NAME_BYTES: usize = 256;
const MAX_TIMESTAMP_BYTES: usize = 96;
const MAX_RAW_MESSAGE_BODY_BYTES: usize = 16 * 1024;
const MAX_PLAIN_MESSAGE_BYTES: usize = 8 * 1024;
// A quote reply produced by Graph contains one `messageReference` attachment
// per quoted message (at most `MAX_REMOTE_QUOTE_SOURCES`). Bound the outer
// collection and opaque JSON string before a second JSON parse so malformed
// metadata cannot confirm an uncertain send. The collection bound leaves room
// for ten references plus a few unrelated attachments, so an over-limit
// quote set is seen and fails closed rather than being truncated.
const MAX_MESSAGE_REFERENCE_ATTACHMENTS: usize = 16;
const MAX_MESSAGE_REFERENCE_ATTACHMENT_FIELDS: usize = 16;
const MAX_MESSAGE_REFERENCE_CONTENT_TYPE_BYTES: usize = 64;
const MAX_MESSAGE_REFERENCE_CONTENT_BYTES: usize = 8 * 1024;
const MAX_MESSAGE_REFERENCE_PREVIEW_BYTES: usize = 512;
const MAX_FILE_REFERENCE_ID_BYTES: usize = 64;
const MAX_FILE_REFERENCE_NAME_BYTES: usize = 512;
const MAX_FILE_REFERENCE_DISPLAY_BYTES: usize = 128;
// Delimiter lookahead is deliberately capped so malformed provider HTML
// cannot turn a run of `<` or `&` characters into repeated full-buffer scans.
const MAX_HTML_TAG_DELIMITER_BYTES: usize = 256;
const MAX_HTML_ENTITY_DELIMITER_BYTES: usize = 16;

/// Directory-refresh-only memo of recent per-chat histories, keyed by chat id
/// and validated against the chat's current last-message preview. Selected
/// history reads and delivery reconciliation never consult it: they must
/// observe Graph, not a remembered answer.
#[derive(Default)]
pub(super) struct ChatHistoryCache {
    account_id: Option<String>,
    entries: BTreeMap<String, CachedChatHistory>,
}

struct CachedChatHistory {
    preview_id: String,
    preview_modified: Option<String>,
    fetched_at: Instant,
    truncated: bool,
    messages: Vec<RemoteChatMessage>,
}

impl ChatHistoryCache {
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.account_id = None;
    }

    #[cfg(test)]
    pub(super) fn seed_for_test(&mut self, chat_id: &str) {
        self.entries.insert(
            chat_id.into(),
            CachedChatHistory {
                preview_id: "p".into(),
                preview_modified: None,
                fetched_at: Instant::now(),
                truncated: false,
                messages: Vec::new(),
            },
        );
    }

    #[cfg(test)]
    pub(super) fn contains_for_test(&self, chat_id: &str) -> bool {
        self.entries.contains_key(chat_id)
    }

    /// Forgets one chat's remembered history after an edit or deletion, so
    /// the next directory refresh cannot present the pre-mutation rows.
    pub(super) fn invalidate(&mut self, chat_id: &str) {
        self.entries.remove(chat_id);
    }

    /// Messages carry current-user markers, so a different account never
    /// reuses another account's entries.
    fn align_to_account(&mut self, account_id: Option<&str>) {
        if self.account_id.as_deref() != account_id {
            self.entries.clear();
            self.account_id = account_id.map(str::to_owned);
        }
    }

    fn lookup(
        &self,
        chat_id: &str,
        preview: Option<&GraphChatMessage>,
        now: Instant,
    ) -> Option<&CachedChatHistory> {
        let preview = preview?;
        let preview_id = preview.id.as_deref()?;
        let entry = self.entries.get(chat_id)?;
        (entry.preview_id == preview_id
            && entry.preview_modified.as_deref() == preview.last_modified_date_time.as_deref()
            && now.saturating_duration_since(entry.fetched_at) < CHAT_HISTORY_CACHE_TTL)
            .then_some(entry)
    }

    fn store(
        &mut self,
        chat_id: &str,
        preview: Option<&GraphChatMessage>,
        history: &MessageHistory,
        now: Instant,
    ) {
        let Some(preview) = preview else { return };
        let Some(preview_id) = preview.id.clone() else {
            return;
        };
        if !self.entries.contains_key(chat_id)
            && self.entries.len() >= MAX_CACHED_CHAT_HISTORIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.fetched_at)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            chat_id.to_owned(),
            CachedChatHistory {
                preview_id,
                preview_modified: preview.last_modified_date_time.clone(),
                fetched_at: now,
                truncated: history.truncated,
                messages: history.messages.clone(),
            },
        );
    }
}

/// A normalized successful chat read plus non-sensitive, presentation-safe
/// warnings about intentionally bounded results.
struct ChatReadResult {
    projection: RemoteChatProjection,
    notices: Vec<String>,
}

/// Failures are category-only: no URL, body, token, or Graph error text can
/// cross this module boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatReadFailure {
    Cancelled,
    ReauthenticationRequired,
    AccessDenied,
    Throttled(Option<u64>),
    Unavailable,
}

impl ChatReadFailure {
    fn transport_error(self) -> TransportError {
        match self {
            Self::Cancelled => TransportError::Cancelled,
            Self::ReauthenticationRequired => TransportError::ReauthenticationRequired,
            Self::AccessDenied => TransportError::Unauthorized,
            Self::Throttled(retry_after_seconds) => TransportError::Throttled {
                retry_after_seconds,
            },
            Self::Unavailable => TransportError::Unavailable(
                "Microsoft Graph chat synchronization is temporarily unavailable".into(),
            ),
        }
    }
}

impl Microsoft365Transport {
    /// Reads the bounded message history for exactly one user-selected remote
    /// chat. This stays separate from the global chat directory so selecting a
    /// conversation does not fan out into unbounded tenant-wide history reads.
    pub(super) fn poll_chat_messages(
        &mut self,
        chat: ProviderResourceRef,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_messages_with_cancellation(chat, None)
    }

    /// Lets the controller stop an obsolete selected-chat read before its
    /// bounded pages can surface a history for a later selection or profile.
    pub(super) fn poll_chat_messages_cancellable(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chat_messages_with_cancellation(chat, Some(cancellation))
    }

    fn poll_chat_messages_with_cancellation(
        &mut self,
        chat: ProviderResourceRef,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        let chat_id = selected_chat_identifier(&chat)?;
        let completed = SyncInterest::ChatMessages { chat: chat.clone() };
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }

        match fetch_recent_messages(
            &self.session,
            chat_id,
            self.identity.as_ref(),
            cancellation,
        ) {
            Ok(history) => {
                let messages = self.with_mention_handles(chat_id, history.messages, &history.mentioned_user_ids);
                let mut notices = Vec::new();
                if history.truncated {
                    notices.push(format!(
                        "Microsoft 365 chat history is showing the newest {MAX_MESSAGES_PER_CHAT} messages for this conversation."
                    ));
                }
                Ok(WorkspaceDelta {
                    completed: vec![completed],
                    identity: None,
                    remote_chats: None,
                    remote_chat_messages: Some(RemoteChatMessagesProjection::microsoft365(
                        chat, messages,
                    )?),
                    remote_pinned_chat_messages: None,
                    remote_chat_members: None,
                    remote_message_search: None,
                    remote_teams: None,
                    remote_channel_messages: None,
                    remote_channel_files: None,
                    remote_channel_replies: None,
                    remote_files: None,
                    remote_file_preview: None,
                    remote_calendar: None,
                    remote_capabilities: None,
                    notices,
                })
            }
            Err(ChatReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // A selected history has its own target-bound access state.
                // Keep the directory and verified identity intact while the
                // feature clears only this conversation's stale history.
                completed: vec![completed],
                identity: None,
                remote_chats: None,
                remote_chat_messages: Some(RemoteChatMessagesProjection::access_denied(chat)?),
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 denied access to this chat's history. Verify delegated Chat.Read access and your chat membership, then reconnect."
                        .into(),
                ],
            }),
            Err(ChatReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(ChatReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => Err(failure.transport_error()),
        }
    }

    /// Gives each message the handles of the members it mentions, using the
    /// roster this adapter remembered for the chat. A mention of anyone not in
    /// that roster (or with no roster read yet) is dropped.
    fn with_mention_handles(
        &self,
        chat_id: &str,
        messages: Vec<RemoteChatMessage>,
        mentioned_user_ids: &[Vec<String>],
    ) -> Vec<RemoteChatMessage> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                let handles = mentioned_user_ids
                    .get(index)
                    .into_iter()
                    .flatten()
                    .filter_map(|user_id| self.member_ids.handle_for_user(chat_id, user_id))
                    .collect();
                message.with_mentioned_handles(handles)
            })
            .collect()
    }

    /// Reads a bounded remote chat projection after identity is already
    /// connected. A missing `Chat.Read` consent returns a user-safe notice
    /// while retaining the verified `/me` connection; it must not make an
    /// otherwise valid identity look signed out.
    pub(super) fn poll_chats(&mut self) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chats_with_cancellation(None)
    }

    /// The background transport worker supplies a cancellation signal so a
    /// profile transition cannot start another bounded Graph request after it
    /// has invalidated the adapter's current session.
    pub(super) fn poll_chats_cancellable(
        &mut self,
        cancellation: &TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.poll_chats_with_cancellation(Some(cancellation))
    }

    fn poll_chats_with_cancellation(
        &mut self,
        cancellation: Option<&TransportCancellation>,
    ) -> Result<WorkspaceDelta, TransportError> {
        if let Err(failure) = check_cancelled(cancellation) {
            return Err(failure.transport_error());
        }
        match fetch_chat_projection(
            &self.session,
            self.identity.as_ref(),
            &mut self.chat_history_cache,
            Instant::now(),
            cancellation,
        ) {
            Ok(result) => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                identity: None,
                remote_chats: Some(result.projection),
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: result.notices,
            }),
            Err(ChatReadFailure::AccessDenied) => Ok(WorkspaceDelta {
                // Mark the interest handled with an empty replacement so a
                // caller can clear stale rows without treating a missing
                // delegated scope as an identity failure.
                completed: vec![SyncInterest::Chats],
                identity: None,
                remote_chats: Some(RemoteChatProjection::microsoft365(Vec::new())),
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_files: None,
                remote_file_preview: None,
                remote_calendar: None,
                remote_capabilities: None,
                notices: vec![
                    "Microsoft 365 chat access was not consented. Add delegated Chat.Read and reconnect to load remote chats."
                        .into(),
                ],
            }),
            Err(ChatReadFailure::ReauthenticationRequired) => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            Err(ChatReadFailure::Cancelled) => Err(TransportError::Cancelled),
            Err(failure) => {
                // A chat feed problem is narrower than the known identity.
                // Preserve `ConnectionStatus::Connected` so the account menu
                // does not conflate a transient/limited Chat.Read surface
                // with a broken sign-in.
                Err(failure.transport_error())
            }
        }
    }
}

pub(super) fn selected_chat_identifier(chat: &ProviderResourceRef) -> Result<&str, TransportError> {
    let chat_id = chat.microsoft365_id().ok_or_else(|| {
        TransportError::Unsupported("Microsoft Graph cannot target a local preview chat".into())
    })?;
    if text::normalized_identifier(chat_id).as_deref() != Some(chat_id) {
        return Err(TransportError::InvalidCommand(
            "Microsoft Graph chat selection contains an invalid provider identifier".into(),
        ));
    }
    Ok(chat_id)
}

fn fetch_chat_projection(
    session: &GraphSession<'_>,
    current_identity: Option<&AccountIdentity>,
    cache: &mut ChatHistoryCache,
    now: Instant,
    cancellation: Option<&TransportCancellation>,
) -> Result<ChatReadResult, ChatReadFailure> {
    check_cancelled(cancellation)?;
    cache.align_to_account(current_identity.map(|identity| identity.account_id.as_str()));
    let (graph_chats, list_truncated) = fetch_chat_pages(session, cancellation)?;
    let mut conversations = Vec::with_capacity(graph_chats.len());
    let mut notices = Vec::new();
    let mut detailed_histories = 0_usize;
    let mut history_was_truncated = false;
    let mut histories_unavailable = 0_usize;
    let mut history_throttle_retry_after: Option<Option<u64>> = None;

    for graph_chat in graph_chats {
        check_cancelled(cancellation)?;
        let Some(chat_id) = normalized_identifier(graph_chat.id) else {
            notices.push(
                "Microsoft Graph returned a chat without a usable identifier; it was skipped."
                    .into(),
            );
            continue;
        };
        let target = ProviderResourceRef::microsoft365(chat_id.clone());
        let kind = remote_chat_kind(graph_chat.chat_type.as_deref());
        let mut messages = graph_chat
            .last_message_preview
            .as_ref()
            .map(|preview| normalize_message(preview, current_identity))
            .into_iter()
            .collect::<Vec<_>>();

        // The list route carries only the last-message preview. Fetch a
        // bounded richer history for the most recent conversations so the
        // first projection is immediately useful without launching an
        // unbounded number of Graph requests. Other rows retain their safe
        // preview; a later selection-driven interest can fill them lazily.
        if detailed_histories < MAX_DETAILED_CHAT_HISTORIES
            && history_throttle_retry_after.is_none()
        {
            detailed_histories += 1;
            // An unchanged preview and a young entry let this refresh skip
            // the history GETs entirely.
            let cached = cache
                .lookup(&chat_id, graph_chat.last_message_preview.as_ref(), now)
                .map(|entry| (entry.messages.clone(), entry.truncated));
            let fetched = match cached {
                Some((messages, truncated)) => Ok(MessageHistory {
                    messages,
                    truncated,
                    mentioned_user_ids: Vec::new(),
                }),
                None => fetch_recent_messages(session, &chat_id, current_identity, cancellation)
                    .inspect(|history| {
                        cache.store(
                            &chat_id,
                            graph_chat.last_message_preview.as_ref(),
                            history,
                            now,
                        );
                    }),
            };
            match fetched {
                Ok(history) => {
                    history_was_truncated |= history.truncated;
                    if !history.messages.is_empty() {
                        messages = history.messages;
                    }
                }
                Err(ChatReadFailure::ReauthenticationRequired) => {
                    return Err(ChatReadFailure::ReauthenticationRequired);
                }
                Err(ChatReadFailure::Cancelled) => return Err(ChatReadFailure::Cancelled),
                Err(ChatReadFailure::AccessDenied) => {
                    // The chat list may be available under a basic scope even
                    // when message content is not. Preserve the preview and
                    // surface one aggregate notice below.
                    histories_unavailable = histories_unavailable.saturating_add(1);
                }
                Err(ChatReadFailure::Throttled(retry_after_seconds)) => {
                    histories_unavailable = histories_unavailable.saturating_add(1);
                    // A nested-history throttle is narrower than the list
                    // projection. Retain previews, but do not amplify a
                    // provider backoff by immediately requesting more chats.
                    history_throttle_retry_after = Some(retry_after_seconds);
                }
                Err(ChatReadFailure::Unavailable) => {
                    histories_unavailable = histories_unavailable.saturating_add(1);
                }
            }
        }

        let title = chat_title(graph_chat.topic.as_deref(), kind);
        let subtitle = chat_subtitle(kind, graph_chat.last_message_preview.as_ref());
        let initials = initials_for(&title);
        let unread = chat_has_unread_preview(
            graph_chat.last_message_preview.as_ref(),
            graph_chat.viewpoint.as_ref(),
        );
        let conversation = RemoteChatConversation::microsoft365(
            target, kind, title, subtitle, initials, unread, messages,
        )
        .map_err(|_| ChatReadFailure::Unavailable)?;
        conversations.push(conversation);
    }

    if list_truncated {
        notices.push(format!(
            "Microsoft 365 chat sync is showing the newest {MAX_REMOTE_CHATS} conversations."
        ));
    }
    if history_was_truncated {
        notices.push(format!(
            "Microsoft 365 chat sync is showing the newest {MAX_MESSAGES_PER_CHAT} messages for some conversations."
        ));
    }
    if let Some(retry_after_seconds) = history_throttle_retry_after {
        notices.push(match retry_after_seconds {
            Some(seconds) => format!(
                "Microsoft 365 chat history sync was throttled; retry after {seconds} seconds."
            ),
            None => "Microsoft 365 chat history sync was throttled; retry later.".into(),
        });
    }
    if histories_unavailable > 0 {
        notices.push(
            "Some Microsoft 365 chat histories could not be read; their last-message previews remain available."
                .into(),
        );
    }

    check_cancelled(cancellation)?;
    Ok(ChatReadResult {
        projection: RemoteChatProjection::microsoft365(conversations),
        notices,
    })
}

fn fetch_chat_pages(
    session: &GraphSession<'_>,
    cancellation: Option<&TransportCancellation>,
) -> Result<(Vec<GraphChat>, bool), ChatReadFailure> {
    let mut next = session
        .cloud()
        .graph_route(&["me", "chats"], Some(GRAPH_CHAT_LIST_QUERY))
        .map_err(|_| ChatReadFailure::Unavailable)?;
    let mut chats = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;

    for page_index in 0..MAX_CHAT_PAGES {
        let page: GraphCollection<GraphChat> = get_collection(session, &next, cancellation)?;
        for chat in page.value {
            let Some(id) = chat.id.as_deref().and_then(text::normalized_identifier) else {
                // Malformed rows do not poison a whole account projection.
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            chats.push(chat);
            if chats.len() == MAX_REMOTE_CHATS {
                truncated = true;
                return Ok((chats, truncated));
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok((chats, truncated));
        };
        next = validated_graph_next_link(session.cloud(), &next_link, next.path())?;
        if page_index + 1 == MAX_CHAT_PAGES {
            truncated = true;
        }
    }
    Ok((chats, truncated))
}

struct MessageHistory {
    messages: Vec<RemoteChatMessage>,
    truncated: bool,
    /// Adapter-private and positional with `messages`: the user IDs each
    /// message mentions, in order. They are turned into opaque handles by the
    /// transport and never stored in a message. Empty when not collected.
    mentioned_user_ids: Vec<Vec<String>>,
}

fn fetch_recent_messages(
    session: &GraphSession<'_>,
    chat_id: &str,
    current_identity: Option<&AccountIdentity>,
    cancellation: Option<&TransportCancellation>,
) -> Result<MessageHistory, ChatReadFailure> {
    let mut next = chat_messages_url(session.cloud(), chat_id)?;
    let expected_path = next.path().to_owned();
    let mut messages = Vec::new();
    let mut truncated = false;

    for page_index in 0..MAX_MESSAGE_PAGES_PER_CHAT {
        let page: GraphCollection<GraphChatMessage> = get_collection(session, &next, cancellation)?;
        for message in page.value {
            messages.push(message);
            if messages.len() == MAX_MESSAGES_PER_CHAT {
                truncated = true;
                return Ok(normalized_message_history(
                    messages,
                    current_identity,
                    truncated,
                ));
            }
        }
        let Some(next_link) = page.next_link else {
            return Ok(normalized_message_history(
                messages,
                current_identity,
                truncated,
            ));
        };
        next = validated_chat_messages_next_link(session.cloud(), &next_link, &expected_path)?;
        if page_index + 1 == MAX_MESSAGE_PAGES_PER_CHAT {
            truncated = true;
        }
    }
    Ok(normalized_message_history(
        messages,
        current_identity,
        truncated,
    ))
}

/// Graph's history route is requested in descending creation order, but a
/// provider can still return a page ordered by a later edit. Normalize every
/// bounded page set by `createdDateTime` before the UI sees it.
fn normalized_message_history(
    mut messages: Vec<GraphChatMessage>,
    current_identity: Option<&AccountIdentity>,
    truncated: bool,
) -> MessageHistory {
    // Parse each timestamp once (not per comparison). A lexical comparison of
    // the raw strings is wrong for mixed fractional digits (`...:00Z` vs
    // `...:00.5Z`) and non-UTC offsets. Unparsable or absent values sort last
    // and keep their relative order because the sort is stable.
    messages.sort_by_cached_key(|message| {
        let created = message
            .created_date_time
            .as_deref()
            .and_then(parse_graph_date_time_offset);
        (created.is_none(), created)
    });
    MessageHistory {
        messages: messages
            .iter()
            .map(|message| normalize_message(message, current_identity))
            .collect(),
        truncated,
        mentioned_user_ids: messages.iter().map(mentioned_user_ids).collect(),
    }
}

/// Determines whether Graph's newest preview is later than its recorded read
/// receipt. DateTimeOffset values are normalized before comparison because a
/// lexical comparison is wrong for otherwise equivalent timezone offsets.
///
/// A malformed or missing timestamp cannot prove the preview was read, so it
/// deliberately remains unread in the local projection. This is safer than
/// hiding a provider notification because of an untrusted DTO field.
fn chat_has_unread_preview(
    preview: Option<&GraphChatMessage>,
    viewpoint: Option<&GraphChatViewpoint>,
) -> bool {
    let Some(preview) = preview else {
        return false;
    };
    let Some(preview_created_at) = preview
        .created_date_time
        .as_deref()
        .and_then(parse_graph_date_time_offset)
    else {
        return true;
    };
    let Some(last_read_at) = viewpoint
        .and_then(|viewpoint| viewpoint.last_message_read_date_time.as_deref())
        .and_then(parse_graph_date_time_offset)
    else {
        return true;
    };
    preview_created_at > last_read_at
}

/// A bounded, strict RFC 3339 DateTimeOffset representation. Graph owns the
/// source format, so the parser rejects malformed input rather than trying to
/// guess a local timezone or silently coerce a timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GraphDateTimeOffset {
    utc_seconds: i64,
    nanoseconds: u32,
}

fn parse_graph_date_time_offset(value: &str) -> Option<GraphDateTimeOffset> {
    let bytes = value.as_bytes();
    if bytes.len() > MAX_TIMESTAMP_BYTES || bytes.len() < 20 || !value.is_ascii() {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }

    let year = parse_fixed_decimal(bytes, 0, 4)? as i64;
    let month = parse_fixed_decimal(bytes, 5, 2)?;
    let day = parse_fixed_decimal(bytes, 8, 2)?;
    let hour = parse_fixed_decimal(bytes, 11, 2)?;
    let minute = parse_fixed_decimal(bytes, 14, 2)?;
    let second = parse_fixed_decimal(bytes, 17, 2)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)?
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut cursor = 19;
    let nanoseconds = if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        let fraction_len = cursor.saturating_sub(fraction_start);
        if !(1..=9).contains(&fraction_len) {
            return None;
        }
        let fraction = parse_fixed_decimal(bytes, fraction_start, fraction_len)?;
        fraction.checked_mul(10_u32.pow((9 - fraction_len) as u32))?
    } else {
        0
    };

    let timezone_offset_seconds = match bytes.get(cursor).copied() {
        Some(b'Z') if cursor + 1 == bytes.len() => 0_i64,
        Some(sign @ (b'+' | b'-')) if cursor + 6 == bytes.len() => {
            if bytes.get(cursor + 3) != Some(&b':') {
                return None;
            }
            let offset_hours = parse_fixed_decimal(bytes, cursor + 1, 2)?;
            let offset_minutes = parse_fixed_decimal(bytes, cursor + 4, 2)?;
            if offset_hours > 23 || offset_minutes > 59 {
                return None;
            }
            let offset = i64::from(offset_hours) * 3_600 + i64::from(offset_minutes) * 60;
            if sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };

    let civil_seconds = days_from_civil(year, month, day)?
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    Some(GraphDateTimeOffset {
        utc_seconds: civil_seconds.checked_sub(timezone_offset_seconds)?,
        nanoseconds,
    })
}

fn parse_fixed_decimal(bytes: &[u8], start: usize, width: usize) -> Option<u32> {
    let digits = bytes.get(start..start.checked_add(width)?)?;
    digits.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value.checked_mul(10)?.checked_add(u32::from(*byte - b'0')))
            .flatten()
    })
}

fn days_in_month(year: i64, month: u32) -> Option<u32> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return None,
    })
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Number of days relative to 1970-01-01, using a proleptic Gregorian
/// calendar. Inputs are already range-checked, and the four-digit year makes
/// the arithmetic comfortably representable in `i64`.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let adjusted_year = year.checked_sub(if month <= 2 { 1 } else { 0 })?;
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year.checked_sub(era.checked_mul(400)?)?;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153_i64.checked_mul(adjusted_month)? + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era
        .checked_mul(365)?
        .checked_add(year_of_era / 4)?
        .checked_sub(year_of_era / 100)?
        .checked_add(day_of_year)?;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

fn chat_messages_url(cloud: GraphCloud, chat_id: &str) -> Result<Url, ChatReadFailure> {
    let mut url = cloud
        .graph_route(&["chats", chat_id, "messages"], None)
        .map_err(|_| ChatReadFailure::Unavailable)?;
    url.query_pairs_mut()
        .append_pair("$top", "50")
        .append_pair("$orderby", "createdDateTime desc");
    Ok(url)
}

fn get_collection<T: DeserializeOwned>(
    session: &GraphSession<'_>,
    url: &Url,
    cancellation: Option<&TransportCancellation>,
) -> Result<GraphCollection<T>, ChatReadFailure> {
    check_cancelled(cancellation)?;
    let response = session
        .get_json(url, MAX_CHAT_RESPONSE_BYTES)
        .map_err(map_http_error)?;
    // The response body zeroizes on drop. Do not deserialize it or initiate
    // the next page when the worker was invalidated while this request ran.
    check_cancelled(cancellation)?;
    validate_chat_response_status(&response)?;
    decode_collection(&response.body)
}

/// Decodes one Graph collection page body. Pure, so fuzz targets and tests can
/// reach the production decode path without a session.
fn decode_collection<T: DeserializeOwned>(
    body: &[u8],
) -> Result<GraphCollection<T>, ChatReadFailure> {
    serde_json::from_slice::<GraphCollection<T>>(body).map_err(|_| ChatReadFailure::Unavailable)
}

/// Fuzzing seam: decodes `body` as every collection this adapter reads and
/// returns the number of rows seen.
#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_decode(body: &[u8]) -> usize {
    let mut count = 0;
    if let Ok(page) = decode_collection::<GraphChat>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    if let Ok(page) = decode_collection::<GraphChatMessage>(body) {
        count += page.value.len() + usize::from(page.next_link.is_some());
    }
    count
}

fn check_cancelled(cancellation: Option<&TransportCancellation>) -> Result<(), ChatReadFailure> {
    cancellation
        .is_none_or(|cancellation| !cancellation.is_cancelled())
        .then_some(())
        .ok_or(ChatReadFailure::Cancelled)
}

fn map_http_error(error: GraphHttpError) -> ChatReadFailure {
    match error {
        GraphHttpError::Network
        | GraphHttpError::ResponseTooLarge
        | GraphHttpError::Unsupported
        | GraphHttpError::RenewedBeforeDelivery
        | GraphHttpError::RenewalUnavailable => ChatReadFailure::Unavailable,
    }
}

fn validate_chat_response_status(response: &GraphHttpResponse) -> Result<(), ChatReadFailure> {
    match response.status {
        200 => Ok(()),
        401 => Err(ChatReadFailure::ReauthenticationRequired),
        403 => Err(ChatReadFailure::AccessDenied),
        429 => Err(ChatReadFailure::Throttled(response.retry_after_seconds)),
        _ => Err(ChatReadFailure::Unavailable),
    }
}

/// Follows the chat-list cursor only on the exact route that was requested;
/// a Graph host and API prefix alone could steer the bearer token elsewhere.
fn validated_graph_next_link(
    cloud: GraphCloud,
    raw: &str,
    expected_path: &str,
) -> Result<Url, ChatReadFailure> {
    cloud
        .validated_next_link(raw, expected_path)
        .ok_or(ChatReadFailure::Unavailable)
}

/// Follows an opaque chat-history cursor only when it stays on the exact
/// selected chat's messages route. A Graph host and API prefix alone are not
/// enough: a hostile or malformed response must not steer the bearer token to
/// another chat or resource path.
fn validated_chat_messages_next_link(
    cloud: GraphCloud,
    raw: &str,
    expected_path: &str,
) -> Result<Url, ChatReadFailure> {
    cloud
        .validated_next_link(raw, expected_path)
        .ok_or(ChatReadFailure::Unavailable)
}

#[derive(Deserialize)]
struct GraphCollection<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChat {
    id: Option<String>,
    chat_type: Option<String>,
    topic: Option<String>,
    viewpoint: Option<GraphChatViewpoint>,
    last_message_preview: Option<GraphChatMessage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChatViewpoint {
    last_message_read_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphChatMessage {
    id: Option<String>,
    body: Option<GraphItemBody>,
    from: Option<GraphChatMessageFrom>,
    created_date_time: Option<String>,
    deleted_date_time: Option<String>,
    last_modified_date_time: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    importance: Option<String>,
    #[serde(default, deserialize_with = "deserialize_mentions")]
    mentions: Decorations<GraphChatMessageMention>,
    #[serde(default, deserialize_with = "deserialize_reactions")]
    reactions: Vec<GraphChatMessageReaction>,
    #[serde(default, deserialize_with = "deserialize_quote_attachments")]
    attachments: Decorations<GraphChatMessageAttachment>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    message_type: Option<String>,
    /// Present (non-null) when a data-loss-prevention policy touched the
    /// message. Only its presence matters; the content is never read.
    #[serde(default)]
    policy_violation: Option<serde::de::IgnoredAny>,
}

/// A decorating collection that remembers whether Graph sent anything at
/// all, even when the lenient parser below dropped every entry. Editing
/// rewrites the whole body, so a message that carries any attachment or
/// mention metadata must be treated as non-editable rather than silently
/// stripped of it.
struct Decorations<T> {
    items: Vec<T>,
    any: bool,
}

impl<T> Default for Decorations<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            any: false,
        }
    }
}

/// True for anything other than null, a missing field, or an empty array.
fn raw_collection_present(value: &Option<serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Array(values)) => !values.is_empty(),
        Some(_) => true,
    }
}

/// The only attachment fields used by this client. It remains an internal
/// narrow DTO so attachment URLs, IDs, and arbitrary Graph content cannot
/// cross into presentation or reconciliation state.
struct GraphChatMessageAttachment {
    content_type: Option<String>,
    content: Option<String>,
    /// The attachment ID and display name of a file reference only; a
    /// `contentUrl` is never read.
    id: Option<String>,
    name: Option<String>,
}

/// Attachments are optional presentation metadata. Preserve the chat row on
/// null/wrong-shape data, but fail the whole quote-reference collection closed
/// so a malformed or duplicate reference cannot hide behind later data. The
/// inner `messageReference.content` is parsed only after its bound is checked.
fn deserialize_quote_attachments<'de, D>(
    deserializer: D,
) -> Result<Decorations<GraphChatMessageAttachment>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    let any = raw_collection_present(&raw);
    Ok(Decorations {
        items: parse_quote_attachments(raw),
        any,
    })
}

fn parse_quote_attachments(raw: Option<serde_json::Value>) -> Vec<GraphChatMessageAttachment> {
    let Some(serde_json::Value::Array(values)) = raw else {
        return Vec::new();
    };
    parse_quote_attachment_values(values)
}

fn parse_quote_attachment_values(
    values: Vec<serde_json::Value>,
) -> Vec<GraphChatMessageAttachment> {
    if values.len() > MAX_MESSAGE_REFERENCE_ATTACHMENTS {
        return Vec::new();
    }
    let mut attachments = Vec::with_capacity(values.len());
    for value in values {
        let serde_json::Value::Object(fields) = value else {
            return Vec::new();
        };
        if fields.len() > MAX_MESSAGE_REFERENCE_ATTACHMENT_FIELDS {
            return Vec::new();
        }
        let content_type = match fields.get("contentType") {
            None => None,
            Some(serde_json::Value::String(value))
                if value.len() <= MAX_MESSAGE_REFERENCE_CONTENT_TYPE_BYTES =>
            {
                Some(value.clone())
            }
            Some(_) => return Vec::new(),
        };
        let content = match fields.get("content") {
            None => None,
            Some(serde_json::Value::String(value))
                if value.len() <= MAX_MESSAGE_REFERENCE_CONTENT_BYTES =>
            {
                Some(value.clone())
            }
            Some(_) => return Vec::new(),
        };
        let short_string = |key: &str, limit: usize| match fields.get(key) {
            Some(serde_json::Value::String(value)) if value.len() <= limit => Some(value.clone()),
            _ => None,
        };
        attachments.push(GraphChatMessageAttachment {
            content_type,
            content,
            id: short_string("id", MAX_FILE_REFERENCE_ID_BYTES),
            name: short_string("name", MAX_FILE_REFERENCE_NAME_BYTES),
        });
    }
    attachments
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageReference {
    message_id: String,
    message_preview: Option<String>,
    message_sender: Option<GraphMessageReferenceSender>,
}

#[derive(Deserialize)]
struct GraphMessageReferenceSender {
    user: Option<GraphMessageReferenceUser>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageReferenceUser {
    display_name: Option<String>,
}

/// Graph's mention DTO has more recipient kinds than this app can use for a
/// current-user quick view. Retain only the mentioned user identity long
/// enough to compare it with the already-verified `/me` account ID; the
/// provider ID itself never leaves this normalization layer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChatMessageMention {
    mentioned: Option<GraphChatMessageMentionedIdentitySet>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChatMessageMentionedIdentitySet {
    user: Option<GraphIdentity>,
}

/// Mentions decorate a message rather than defining whether its safe text can
/// render. Graph has changed this optional shape over time, so malformed,
/// null, or non-array mention metadata is discarded rather than failing an
/// otherwise valid bounded history. Every surviving entry is deserialized
/// independently and still has to pass the exact verified-account comparison
/// below.
fn deserialize_mentions<'de, D>(
    deserializer: D,
) -> Result<Decorations<GraphChatMessageMention>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    let any = raw_collection_present(&raw);
    let Some(serde_json::Value::Array(values)) = raw else {
        return Ok(Decorations {
            items: Vec::new(),
            any,
        });
    };
    Ok(Decorations {
        items: values
            .into_iter()
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect(),
        any,
    })
}

/// The user IDs a live message mentions, in order, duplicates kept and
/// malformed entries skipped. A deleted message mentions no one. Adapter
/// only: the caller maps these to opaque handles.
fn mentioned_user_ids(message: &GraphChatMessage) -> Vec<String> {
    if message.deleted_date_time.is_some() {
        return Vec::new();
    }
    message
        .mentions
        .items
        .iter()
        .filter_map(|mention| {
            mention
                .mentioned
                .as_ref()
                .and_then(|mentioned| mentioned.user.as_ref())
                .and_then(|user| user.id.as_deref())
                .and_then(text::normalized_identifier)
        })
        .collect()
}

/// Optional priority is presentation metadata, not a reason to discard an
/// otherwise safe chat row. Graph can send null or a malformed future shape;
/// preserve only actual strings and let normalization default the rest.
fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::String(value)) => Some(value),
            None | Some(_) => None,
        },
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChatMessageReaction {
    reaction_type: Option<String>,
    user: Option<GraphChatMessageFrom>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphItemBody {
    content_type: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphChatMessageFrom {
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

pub(super) fn normalize_message(
    message: &GraphChatMessage,
    current_identity: Option<&AccountIdentity>,
) -> RemoteChatMessage {
    // A malformed or absent ID does not make otherwise safe preview text
    // disappear, but it must never become a mutation target.
    let target = message
        .id
        .as_deref()
        .and_then(text::normalized_identifier)
        .map(ProviderResourceRef::microsoft365);
    let deleted = message.deleted_date_time.is_some();
    let (sender_name, sender_id) = sender_details(message.from.as_ref());
    let sent_by_current_user = current_identity
        .is_some_and(|identity| sender_id.as_deref() == Some(identity.account_id.as_str()));
    let mentioned_current_user = current_identity.is_some_and(|identity| {
        message.mentions.items.iter().any(|mention| {
            mention
                .mentioned
                .as_ref()
                .and_then(|mentioned| mentioned.user.as_ref())
                .and_then(|user| user.id.as_deref())
                == Some(identity.account_id.as_str())
        })
    });
    let importance = normalize_message_importance(message.importance.as_deref());
    let sent_at = message
        .created_date_time
        .as_deref()
        .map(|value| safe_display(value, MAX_TIMESTAMP_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unknown time".into());
    let plain_text = if deleted {
        "This message was deleted.".into()
    } else {
        normalize_body(message.body.as_ref())
    };
    let reactions = normalize_reaction_summaries(
        &message.reactions,
        current_identity.map(|identity| identity.account_id.as_str()),
        |reaction| reaction.reaction_type.as_deref(),
        |reaction| {
            reaction
                .user
                .as_ref()
                .and_then(|identity| identity.user.as_ref())
                .and_then(|user| user.id.as_deref())
        },
    );
    let quotes = normalize_message_references(&message.attachments.items);
    let file_references = normalize_file_references(&message.attachments.items);
    let editable =
        editable::is_plain_text_editable(message, sent_by_current_user, deleted, target.is_some());
    let rich = if deleted {
        None
    } else {
        rich_body_runs(message.body.as_ref())
    };
    RemoteChatMessage::microsoft365(
        target,
        sender_name,
        sent_at,
        plain_text,
        sent_by_current_user,
        mentioned_current_user,
        importance,
        deleted,
        reactions,
        quotes,
    )
    .with_editable(editable)
    .with_rich(rich)
    .with_attachments(file_references)
}

/// Formatted runs of an HTML body; `None` for text bodies and for HTML with
/// no allow-listed formatting, which render from the plain text.
fn rich_body_runs(body: Option<&GraphItemBody>) -> Option<Vec<RichRun>> {
    let body = body?;
    if !body
        .content_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("html"))
    {
        return None;
    }
    rich_runs::html_to_rich_runs(body.content.as_deref()?)
}

/// Returns a validated, canonical Graph message identifier without exposing
/// its raw field generally to sibling adapters. A pinned-message response
/// uses this to prove its outer pin ID and expanded message ID are literally
/// the same provider resource before a presentation row crosses the seam.
pub(super) fn valid_message_identifier(message: &GraphChatMessage) -> Option<&str> {
    let identifier = message.id.as_deref()?;
    (text::normalized_identifier(identifier).as_deref() == Some(identifier)).then_some(identifier)
}

/// Parses every bounded `messageReference` attachment into safe quote
/// previews and opaque source-message targets, in attachment order. Any
/// malformed, null, wrong-shape, or duplicate reference, or more than
/// `MAX_REMOTE_QUOTE_SOURCES` references, fails the whole set closed to an
/// empty list; a normal row is safer than confirming a non-idempotent quote
/// reply by an incomplete source set.
fn normalize_message_references(
    attachments: &[GraphChatMessageAttachment],
) -> Vec<RemoteChatMessageQuote> {
    let mut references: Vec<RemoteChatMessageQuote> = Vec::new();
    for attachment in attachments {
        if attachment.content_type.as_deref() != Some("messageReference") {
            continue;
        }
        if references.len() >= MAX_REMOTE_QUOTE_SOURCES {
            return Vec::new();
        }
        let Some(reference) = normalize_one_message_reference(attachment) else {
            return Vec::new();
        };
        // An identical source cannot prove which set the command targeted.
        if references
            .iter()
            .any(|existing| existing.target() == reference.target())
        {
            return Vec::new();
        }
        references.push(reference);
    }
    references
}

/// The file references of a received message: each `reference` attachment
/// becomes a read-only display name plus its attachment ID (a GUID, which
/// confirms an ambiguous send). A reference without a usable GUID or name is
/// skipped. The `contentUrl` is never read, so no file link can leave the
/// adapter.
fn normalize_file_references(
    attachments: &[GraphChatMessageAttachment],
) -> Vec<RemoteChatAttachment> {
    attachments
        .iter()
        .filter(|attachment| attachment.content_type.as_deref() == Some("reference"))
        .filter_map(|attachment| {
            let id = attachment.id.as_deref()?.trim().to_ascii_lowercase();
            if !super::rich_html::is_attachment_guid(&id) {
                return None;
            }
            let name = safe_display(
                attachment.name.as_deref()?,
                MAX_FILE_REFERENCE_DISPLAY_BYTES,
            );
            (!name.is_empty()).then(|| RemoteChatAttachment::microsoft365(id, name))
        })
        .take(MAX_REMOTE_CHAT_ATTACHMENTS)
        .collect()
}

fn normalize_one_message_reference(
    attachment: &GraphChatMessageAttachment,
) -> Option<RemoteChatMessageQuote> {
    let content = attachment.content.as_deref()?;
    if content.is_empty() || content.len() > MAX_MESSAGE_REFERENCE_CONTENT_BYTES {
        return None;
    }
    let parsed = serde_json::from_str::<GraphMessageReference>(content).ok()?;
    let message_id = text::normalized_identifier(&parsed.message_id)?;
    let sender_name = parsed
        .message_sender
        .as_ref()
        .and_then(|sender| sender.user.as_ref())
        .and_then(|user| user.display_name.as_deref())
        .map(|name| safe_display(name, MAX_SENDER_NAME_BYTES))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Microsoft 365 user".into());
    // Graph's reference preview may contain HTML. Normalize it through the
    // same inert text boundary as message bodies before applying the smaller
    // quote-context display cap.
    let preview = truncate_utf8(
        &html_to_plain_text(parsed.message_preview.as_deref().unwrap_or_default()),
        MAX_MESSAGE_REFERENCE_PREVIEW_BYTES,
    );
    let preview = if preview.is_empty() {
        "Quoted message".into()
    } else {
        preview
    };
    Some(RemoteChatMessageQuote::microsoft365(
        ProviderResourceRef::microsoft365(message_id),
        sender_name,
        preview,
    ))
}

/// Only Graph's exact, documented enum strings become a visible priority.
/// Missing, future, malformed, or case-variant values fail closed to the
/// default rather than becoming an unbounded display or delivery value.
fn normalize_message_importance(raw: Option<&str>) -> RemoteChatMessageImportance {
    match raw {
        Some("high") => RemoteChatMessageImportance::High,
        Some("urgent") => RemoteChatMessageImportance::Urgent,
        Some("normal") | None | Some(_) => RemoteChatMessageImportance::Normal,
    }
}

fn sender_details(from: Option<&GraphChatMessageFrom>) -> (String, Option<String>) {
    let identity = from.and_then(|from| {
        from.user
            .as_ref()
            .or(from.application.as_ref())
            .or(from.device.as_ref())
    });
    let name = identity
        .and_then(|identity| identity.display_name.as_deref())
        .map(|value| safe_display(value, MAX_SENDER_NAME_BYTES))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Microsoft 365".into());
    let id = identity
        .and_then(|identity| identity.id.as_deref())
        .and_then(text::normalized_identifier);
    (name, id)
}

fn normalize_body(body: Option<&GraphItemBody>) -> String {
    normalize_graph_message_body(
        body.and_then(|body| body.content_type.as_deref()),
        body.and_then(|body| body.content.as_deref()),
    )
}

/// Normalizes the `body` fields shared by Graph `chatMessage` resources into
/// bounded inert text. Channel-message reads use this same implementation so
/// markup and malformed input never receive subtly different treatment by
/// two sibling Graph adapters.
pub(super) fn normalize_graph_message_body(
    content_type: Option<&str>,
    content: Option<&str>,
) -> String {
    let raw = content.unwrap_or_default();
    let plain =
        if content_type.is_some_and(|content_type| content_type.eq_ignore_ascii_case("html")) {
            html_to_plain_text(raw)
        } else {
            safe_display(raw, MAX_PLAIN_MESSAGE_BYTES)
        };
    if plain.is_empty() {
        "Shared a message".into()
    } else {
        plain
    }
}

fn remote_chat_kind(raw: Option<&str>) -> RemoteChatKind {
    match raw {
        Some(value) if value.eq_ignore_ascii_case("oneOnOne") => RemoteChatKind::OneOnOne,
        Some(value) if value.eq_ignore_ascii_case("group") => RemoteChatKind::Group,
        Some(value) if value.eq_ignore_ascii_case("meeting") => RemoteChatKind::Meeting,
        _ => RemoteChatKind::Unknown,
    }
}

fn chat_title(topic: Option<&str>, kind: RemoteChatKind) -> String {
    topic
        .map(|topic| safe_display(topic, MAX_CHAT_TITLE_BYTES))
        .filter(|topic| !topic.is_empty())
        .unwrap_or_else(|| match kind {
            RemoteChatKind::OneOnOne => "One-to-one chat".into(),
            RemoteChatKind::Group => "Group chat".into(),
            RemoteChatKind::Meeting => "Meeting chat".into(),
            RemoteChatKind::Unknown => "Microsoft 365 chat".into(),
        })
}

fn chat_subtitle(kind: RemoteChatKind, preview: Option<&GraphChatMessage>) -> String {
    let kind_label = match kind {
        RemoteChatKind::OneOnOne => "One-to-one chat",
        RemoteChatKind::Group => "Group chat",
        RemoteChatKind::Meeting => "Meeting chat",
        RemoteChatKind::Unknown => "Microsoft 365 chat",
    };
    let sender = preview
        .map(|message| sender_details(message.from.as_ref()).0)
        .filter(|sender| sender != "Microsoft 365");
    let subtitle = sender
        .map(|sender| format!("{kind_label} · Latest from {sender}"))
        .unwrap_or_else(|| format!("{kind_label} · Microsoft 365"));
    safe_display(&subtitle, MAX_CHAT_SUBTITLE_BYTES)
}

fn initials_for(name: &str) -> String {
    let mut initials = String::new();
    for word in name.split_whitespace() {
        if let Some(character) = word.chars().next() {
            initials.extend(character.to_uppercase());
            if initials.chars().count() >= 2 {
                break;
            }
        }
    }
    if initials.is_empty() {
        "M".into()
    } else {
        safe_display(&initials, 8)
    }
}

fn normalized_identifier(value: Option<String>) -> Option<String> {
    value.and_then(|value| text::normalized_identifier(&value))
}

/// Converts Graph HTML into inert UI text. Egui never receives raw markup.
/// The parser is intentionally small and conservative: it preserves text and
/// common entities, removes tags, omits script/style contents, then folds all
/// whitespace into a readable single line. Each delimiter search is bounded,
/// and omitted script/style contents are consumed in one pass.
pub(super) fn html_to_plain_text(raw: &str) -> String {
    let raw = truncate_utf8(raw, MAX_RAW_MESSAGE_BODY_BYTES);
    let mut output = String::new();
    let mut index = 0_usize;
    let mut omitted_element: Option<OmittedHtmlElement> = None;

    while index < raw.len() {
        if let Some(element) = omitted_element {
            if raw.as_bytes()[index] == b'<'
                && let Some(close) =
                    bounded_ascii_delimiter(&raw, index + 1, b'>', MAX_HTML_TAG_DELIMITER_BYTES)
            {
                let tag = &raw[index + 1..close];
                if element.is_closing_tag(tag) {
                    omitted_element = None;
                    output.push(' ');
                    index = close + 1;
                    continue;
                }
                // Tags inside an omitted element carry no presentation text;
                // skip a whole bounded tag rather than scanning its contents
                // again one delimiter at a time.
                index = close + 1;
                continue;
            }
            let character = raw[index..]
                .chars()
                .next()
                .expect("a nonempty UTF-8 slice has a character");
            index += character.len_utf8();
            continue;
        }

        if raw.as_bytes()[index] == b'<' {
            if let Some(close) =
                bounded_ascii_delimiter(&raw, index + 1, b'>', MAX_HTML_TAG_DELIMITER_BYTES)
            {
                let tag = &raw[index + 1..close];
                let normalized = tag.trim().trim_start_matches('/').to_ascii_lowercase();
                if let Some(element) = OmittedHtmlElement::from_opening_tag(tag) {
                    omitted_element = Some(element);
                    index = close + 1;
                    continue;
                }
                if matches!(
                    normalized.as_str(),
                    "br" | "br/" | "p" | "div" | "li" | "tr" | "blockquote" | "hr"
                ) {
                    output.push(' ');
                }
                index = close + 1;
                continue;
            }
            output.push(' ');
            index += 1;
            continue;
        }
        if raw.as_bytes()[index] == b'&'
            && let Some(semicolon) =
                bounded_ascii_delimiter(&raw, index + 1, b';', MAX_HTML_ENTITY_DELIMITER_BYTES)
        {
            let entity = &raw[index + 1..semicolon];
            if let Some(character) = decode_entity(entity) {
                output.push(character);
                index = semicolon + 1;
                continue;
            }
        }
        let character = raw[index..]
            .chars()
            .next()
            .expect("a nonempty UTF-8 slice has a character");
        output.push(character);
        index += character.len_utf8();
    }

    safe_display(&output, MAX_PLAIN_MESSAGE_BYTES)
}

#[derive(Clone, Copy)]
enum OmittedHtmlElement {
    Script,
    Style,
}

impl OmittedHtmlElement {
    fn from_opening_tag(tag: &str) -> Option<Self> {
        let normalized = tag.trim().to_ascii_lowercase();
        if normalized.starts_with('/') || normalized.ends_with('/') {
            return None;
        }
        if html_tag_has_name(&normalized, "script") {
            Some(Self::Script)
        } else if html_tag_has_name(&normalized, "style") {
            Some(Self::Style)
        } else {
            None
        }
    }

    fn is_closing_tag(self, tag: &str) -> bool {
        let normalized = tag.trim().to_ascii_lowercase();
        let Some(tag) = normalized.strip_prefix('/') else {
            return false;
        };
        html_tag_has_name(tag.trim_start(), self.name())
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Style => "style",
        }
    }
}

fn html_tag_has_name(tag: &str, name: &str) -> bool {
    let Some(suffix) = tag.strip_prefix(name) else {
        return false;
    };
    let Some(first) = suffix.as_bytes().first() else {
        return true;
    };
    *first == b'/' || first.is_ascii_whitespace()
}

fn bounded_ascii_delimiter(
    value: &str,
    start: usize,
    delimiter: u8,
    max_bytes: usize,
) -> Option<usize> {
    let end = start.saturating_add(max_bytes).min(value.len());
    value.as_bytes()[start..end]
        .iter()
        .position(|byte| *byte == delimiter)
        .map(|offset| start + offset)
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let value = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                });
            value.and_then(char::from_u32)
        }
    }
}

mod editable;
mod rich_runs;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod decode_collection_tests {
    use super::{ChatReadFailure, GraphCollection, decode_collection};

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
                Err(ChatReadFailure::Unavailable)
            ));
        }
    }
}
