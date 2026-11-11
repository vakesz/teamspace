use super::{
    CommandTargetProvider, ProviderResourceRef, RemoteMessageInstant, RichRun, TransportError,
};
use crate::integrations::reaction::RemoteMessageReaction;
use std::fmt;

/// A provider-normalized remote chat snapshot.
///
/// This is deliberately ephemeral: features may render it and retain opaque
/// targets for subsequent workspace commands, but must not fold it into the
/// local-preview persistence format. Microsoft Graph DTOs, URLs, pagination
/// cursors, HTML, and raw provider response fields stay behind the
/// integrations seam.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteChatProjection {
    conversations: Vec<RemoteChatConversation>,
}

impl RemoteChatProjection {
    /// Constructs a snapshot only within the integrations module, after an
    /// adapter has normalized and validated every provider response.
    pub(in crate::integrations) fn microsoft365(
        conversations: Vec<RemoteChatConversation>,
    ) -> Self {
        Self { conversations }
    }

    /// Returns presentation-safe conversations and their opaque send targets.
    /// The target itself has no identifier accessor outside integrations.
    pub(crate) fn conversations(&self) -> &[RemoteChatConversation] {
        &self.conversations
    }

    #[cfg(test)]
    pub(crate) const fn is_empty(&self) -> bool {
        self.conversations.is_empty()
    }

    /// Builds a normalized remote snapshot for feature tests without making
    /// the provider constructor available to production callers.
    #[cfg(test)]
    pub(crate) fn for_test(conversations: Vec<RemoteChatConversation>) -> Self {
        Self::microsoft365(conversations)
    }
}

/// A single remote chat suitable for feature presentation and future send
/// dispatch. Its provider target is intentionally opaque to feature and app
/// code: they can preserve or pass it to a workspace command, but cannot
/// manufacture a Microsoft 365 ID from a fixture key or display label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChatConversation {
    target: ProviderResourceRef,
    kind: RemoteChatKind,
    title: String,
    subtitle: String,
    initials: String,
    unread: bool,
    messages: Vec<RemoteChatMessage>,
}

impl RemoteChatConversation {
    /// Creates a remote chat only after an integration adapter has supplied a
    /// Microsoft 365-owned target. The fallible check exists both as defense
    /// in depth and as a regression guard against a future adapter passing a
    /// local preview reference into a remote projection.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        kind: RemoteChatKind,
        title: String,
        subtitle: String,
        initials: String,
        unread: bool,
        messages: Vec<RemoteChatMessage>,
    ) -> Result<Self, TransportError> {
        target.require_microsoft365("chat")?;
        target.validate("chat id")?;
        Ok(Self {
            target,
            kind,
            title,
            subtitle,
            initials,
            unread,
            messages,
        })
    }

    /// An opaque reference for `WorkspaceCommand::SendChatMessage`. It can
    /// never expose or rebrand the provider-issued identifier outside
    /// integrations.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) const fn kind(&self) -> RemoteChatKind {
        self.kind
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn subtitle(&self) -> &str {
        &self.subtitle
    }

    pub(crate) fn initials(&self) -> &str {
        &self.initials
    }

    pub(crate) const fn is_unread(&self) -> bool {
        self.unread
    }

    pub(crate) fn messages(&self) -> &[RemoteChatMessage] {
        &self.messages
    }

    /// Builds an opaque Microsoft 365 target for tests while retaining the
    /// production rule that only integrations may construct one.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        provider_id: impl Into<String>,
        kind: RemoteChatKind,
        title: impl Into<String>,
        subtitle: impl Into<String>,
        initials: impl Into<String>,
        unread: bool,
        messages: Vec<RemoteChatMessage>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(provider_id),
            kind,
            title.into(),
            subtitle.into(),
            initials.into(),
            unread,
            messages,
        )
        .expect("test remote chat targets must be valid Microsoft 365 identifiers")
    }
}

/// Presentation-safe classification of a remote chat. It intentionally does
/// not mirror provider-specific enum values or expose meeting metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteChatKind {
    OneOnOne,
    Group,
    Meeting,
    Unknown,
}

/// The bounded priority set Microsoft Graph accepts for a chat message.
///
/// This remains a typed value through the feature, controller, and adapter
/// seams so a display label or an unrecognized provider string can never be
/// turned into a delivery payload. `Normal` is both Graph's default and this
/// app's safe fallback for absent or malformed remote metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RemoteChatMessageImportance {
    #[default]
    Normal,
    High,
    Urgent,
}

impl RemoteChatMessageImportance {
    pub(crate) const ALL: [Self; 3] = [Self::Normal, Self::High, Self::Urgent];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::High => "High",
            Self::Urgent => "Urgent",
        }
    }

    /// The only exact wire values the Microsoft Graph adapter may serialize.
    pub(in crate::integrations) const fn graph_value(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }

    /// The default priority does not need a visual marker, while the other
    /// two are intentionally visible beside a remote message.
    pub(crate) const fn badge_label(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::High => Some("High"),
            Self::Urgent => Some("Urgent"),
        }
    }
}

/// A bounded, display-safe reference carried by Graph's `messageReference`
/// attachment after a quote reply has been normalized. Its target is opaque:
/// the feature may compare or preserve it to reconcile one quote reply, but
/// cannot display or manufacture a provider ID from it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteChatMessageQuote {
    target: ProviderResourceRef,
    sender_name: String,
    preview: String,
}

impl fmt::Debug for RemoteChatMessageQuote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteChatMessageQuote")
            .field("has_target", &true)
            .field("sender_name", &self.sender_name)
            .field("preview", &self.preview)
            .finish()
    }
}

impl RemoteChatMessageQuote {
    /// Constructs a normalized quote reference only within an integration
    /// adapter after it has validated the opaque quoted-message target and
    /// bounded all presentation text.
    pub(in crate::integrations) fn microsoft365(
        target: ProviderResourceRef,
        sender_name: String,
        preview: String,
    ) -> Self {
        debug_assert!(
            target.target_provider() == CommandTargetProvider::Microsoft365
                && target.validate("quoted chat message id").is_ok()
        );
        Self {
            target,
            sender_name,
            preview,
        }
    }

    /// The provider-owned source-message target remains opaque outside the
    /// integrations seam. It exists solely for exact quote-reply routing and
    /// reconciliation, never for generic message mutation.
    pub(crate) fn target(&self) -> ProviderResourceRef {
        self.target.clone()
    }

    pub(crate) fn sender_name(&self) -> &str {
        &self.sender_name
    }

    pub(crate) fn preview(&self) -> &str {
        &self.preview
    }
}

/// A read-only normalized message. Its optional opaque provider target can
/// address only the separately scoped selected-chat reaction command; it
/// does not confer editing, deletion, attachments, or generic mutation
/// capability. Graph HTML must already be converted to safe plain text before
/// this type is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChatMessage {
    target: Option<ProviderResourceRef>,
    sender_name: String,
    sent_at: String,
    /// Parsed once at the seam from the provider timestamp, so features can
    /// compare arrivals without parsing display text. `None` when unknown.
    sent_instant: Option<RemoteMessageInstant>,
    plain_text: String,
    sent_by_current_user: bool,
    mentioned_current_user: bool,
    importance: RemoteChatMessageImportance,
    deleted: bool,
    reactions: Vec<RemoteMessageReaction>,
    quotes: Vec<RemoteChatMessageQuote>,
    plain_text_editable: bool,
    /// Formatted runs of an HTML body, only when the body used an
    /// allow-listed mark or link. `plain_text` stays the authoritative text.
    rich: Option<Vec<RichRun>>,
    /// The members this message mentions, in order (a repeated mention
    /// repeats), as handles for the chat it was read from. A mention of
    /// anyone not in the remembered roster is dropped.
    mentioned_handles: Vec<MentionHandle>,
    /// File references the message carries, read-only. See
    /// [`RemoteChatAttachment`].
    attachments: Vec<RemoteChatAttachment>,
}

/// One file reference of a received message, reduced to a display name and
/// the attachment ID (the GUID of the file's ETag). The URL, the drive item
/// and every other provider value are dropped in the adapter. Only a sent
/// message's own reference ID is ever compared, to confirm an ambiguous send.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteChatAttachment {
    id: String,
    name: String,
}

impl std::fmt::Debug for RemoteChatAttachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteChatAttachment")
            .finish_non_exhaustive()
    }
}

/// The most file references one message shows.
pub(in crate::integrations) const MAX_REMOTE_CHAT_ATTACHMENTS: usize = 10;

impl RemoteChatAttachment {
    pub(in crate::integrations) fn microsoft365(id: String, name: String) -> Self {
        Self { id, name }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl RemoteChatMessage {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::integrations) fn microsoft365(
        target: Option<ProviderResourceRef>,
        sender_name: String,
        sent_at: String,
        plain_text: String,
        sent_by_current_user: bool,
        mentioned_current_user: bool,
        importance: RemoteChatMessageImportance,
        deleted: bool,
        reactions: Vec<RemoteMessageReaction>,
        quotes: Vec<RemoteChatMessageQuote>,
    ) -> Self {
        debug_assert!(target.as_ref().is_none_or(|target| {
            target.target_provider() == CommandTargetProvider::Microsoft365
                && target.validate("chat message id").is_ok()
        }));
        Self {
            target,
            sender_name,
            sent_instant: RemoteMessageInstant::parse(&sent_at),
            sent_at,
            plain_text,
            sent_by_current_user,
            mentioned_current_user,
            importance,
            deleted,
            reactions,
            quotes,
            plain_text_editable: false,
            rich: None,
            mentioned_handles: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// Attaches the message's file references (at most
    /// [`MAX_REMOTE_CHAT_ATTACHMENTS`]). Only the integration sets them.
    pub(in crate::integrations) fn with_attachments(
        mut self,
        mut attachments: Vec<RemoteChatAttachment>,
    ) -> Self {
        attachments.truncate(MAX_REMOTE_CHAT_ATTACHMENTS);
        self.attachments = attachments;
        self
    }

    /// The file references this message carries, in Graph's order.
    pub(crate) fn attachments(&self) -> &[RemoteChatAttachment] {
        &self.attachments
    }

    /// Test-only: gives a fixture file references.
    #[cfg(test)]
    pub(crate) fn with_attachments_for_test(self, attachments: Vec<(&str, &str)>) -> Self {
        self.with_attachments(
            attachments
                .into_iter()
                .map(|(id, name)| RemoteChatAttachment::microsoft365(id.into(), name.into()))
                .collect(),
        )
    }

    /// Attaches the resolved mention handles (at most
    /// [`MAX_MENTIONS_PER_MESSAGE`]). Only the integration sets them.
    pub(in crate::integrations) fn with_mentioned_handles(
        mut self,
        mut handles: Vec<MentionHandle>,
    ) -> Self {
        handles.truncate(MAX_MENTIONS_PER_MESSAGE);
        self.mentioned_handles = handles;
        self
    }

    /// The handles of the members this message mentions.
    pub(crate) fn mentioned_handles(&self) -> &[MentionHandle] {
        &self.mentioned_handles
    }

    /// Attaches the formatted runs of a body that used allow-listed markup.
    /// Only the integration's normalization sets it.
    pub(in crate::integrations) fn with_rich(mut self, rich: Option<Vec<RichRun>>) -> Self {
        self.rich = rich;
        self
    }

    /// The bounded formatted runs, when the body had any formatting.
    pub(crate) fn rich(&self) -> Option<&[RichRun]> {
        self.rich.as_deref()
    }

    /// Test-only: gives a fixture the mention handles Graph read back.
    #[cfg(test)]
    pub(crate) fn with_mentioned_handles_for_test(self, handles: Vec<MentionHandle>) -> Self {
        self.with_mentioned_handles(handles)
    }

    /// Test-only: gives a fixture formatted runs.
    #[cfg(test)]
    pub(crate) fn with_rich_for_test(self, rich: Vec<RichRun>) -> Self {
        self.with_rich(Some(rich))
    }

    /// Marks a row the adapter proved can be rewritten as plain text without
    /// losing content. Only the integration's normalization sets it.
    pub(in crate::integrations) fn with_editable(mut self, editable: bool) -> Self {
        self.plain_text_editable = editable;
        self
    }

    /// Whether editing this row as plain text cannot discard formatting,
    /// attachments or mentions. The row must also be the user's own.
    pub(crate) const fn is_plain_text_editable(&self) -> bool {
        self.plain_text_editable
    }

    /// The provider-owned message target is absent only when Graph returned a
    /// malformed or missing identifier. Such a row remains safe to display,
    /// but cannot be turned into a reaction mutation.
    pub(crate) fn target(&self) -> Option<ProviderResourceRef> {
        self.target.clone()
    }

    pub(crate) fn sender_name(&self) -> &str {
        &self.sender_name
    }

    pub(crate) fn sent_at(&self) -> &str {
        &self.sent_at
    }

    pub(crate) const fn sent_instant(&self) -> Option<RemoteMessageInstant> {
        self.sent_instant
    }

    pub(crate) fn plain_text(&self) -> &str {
        &self.plain_text
    }

    pub(crate) const fn sent_by_current_user(&self) -> bool {
        self.sent_by_current_user
    }

    /// Whether Graph's structured mention collection identifies the verified
    /// signed-in account as a recipient of this message. This deliberately
    /// carries no provider identity or display label into feature state, so a
    /// Mention view never needs to infer recipients from rendered text.
    pub(crate) const fn mentions_current_user(&self) -> bool {
        self.mentioned_current_user
    }

    /// The provider-normalized priority of this message. Unknown provider
    /// values normalize to `Normal` before they reach this seam.
    pub(crate) const fn importance(&self) -> RemoteChatMessageImportance {
        self.importance
    }

    pub(crate) const fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// The bounded, application-supported reaction summaries reported by the
    /// selected chat feed. They remain presentation-only until a feature
    /// issues the corresponding exact typed chat/message command.
    pub(crate) fn reactions(&self) -> &[RemoteMessageReaction] {
        &self.reactions
    }

    /// The strictly normalized message-reference attachments, in Graph's
    /// order. A malformed, duplicate, or more-than-ten set is dropped whole,
    /// so it remains empty and cannot confirm an ambiguous quote reply.
    pub(crate) fn quotes(&self) -> &[RemoteChatMessageQuote] {
        &self.quotes
    }

    /// Test-only counterpart to the integration-owned normalization
    /// constructor. It cannot exist in a production build.
    #[cfg(test)]
    pub(crate) fn for_test(
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        deleted: bool,
    ) -> Self {
        Self::microsoft365(
            None,
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            sent_by_current_user,
            false,
            RemoteChatMessageImportance::Normal,
            deleted,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Builds a message fixture with an explicit current-user mention bit.
    /// This is intentionally test-only: production code may establish the
    /// bit only from the integration-owned Graph mention DTO.
    #[cfg(test)]
    pub(crate) fn for_test_with_current_user_mention(
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        mentioned_current_user: bool,
        deleted: bool,
    ) -> Self {
        Self::microsoft365(
            None,
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            sent_by_current_user,
            mentioned_current_user,
            RemoteChatMessageImportance::Normal,
            deleted,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Builds a message fixture that has both a provider message target and
    /// an explicit current-user mention bit.
    #[cfg(test)]
    pub(crate) fn for_test_with_id_and_current_user_mention(
        provider_id: impl Into<String>,
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        mentioned_current_user: bool,
    ) -> Self {
        Self::microsoft365(
            Some(ProviderResourceRef::microsoft365(provider_id)),
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            false,
            mentioned_current_user,
            RemoteChatMessageImportance::Normal,
            false,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Builds an own, targeted message fixture for edit and delete flows.
    #[cfg(test)]
    pub(crate) fn for_test_own(
        provider_id: impl Into<String>,
        plain_text: impl Into<String>,
        editable: bool,
        deleted: bool,
    ) -> Self {
        Self::microsoft365(
            Some(ProviderResourceRef::microsoft365(provider_id)),
            "Me".into(),
            "now".into(),
            plain_text.into(),
            true,
            false,
            RemoteChatMessageImportance::Normal,
            deleted,
            Vec::new(),
            Vec::new(),
        )
        .with_editable(editable)
    }

    /// Builds a message fixture with an explicit normalized importance.
    /// Production callers may receive this value only through the integration
    /// adapter's strict Graph normalization.
    #[cfg(test)]
    pub(crate) fn for_test_with_importance(
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        importance: RemoteChatMessageImportance,
        deleted: bool,
    ) -> Self {
        Self::microsoft365(
            None,
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            sent_by_current_user,
            false,
            importance,
            deleted,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Builds a reaction-capable message fixture with a full typed summary
    /// list. Production callers still cannot manufacture provider IDs.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test_with_reactions(
        provider_id: impl Into<String>,
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        deleted: bool,
        reactions: Vec<RemoteMessageReaction>,
    ) -> Self {
        Self::microsoft365(
            Some(ProviderResourceRef::microsoft365(provider_id)),
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            sent_by_current_user,
            false,
            RemoteChatMessageImportance::Normal,
            deleted,
            reactions,
            Vec::new(),
        )
    }

    /// Builds a quote-reference fixture without making raw provider IDs
    /// constructible in production code.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test_with_quote_reference(
        provider_id: impl Into<String>,
        sender_name: impl Into<String>,
        sent_at: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        importance: RemoteChatMessageImportance,
        deleted: bool,
        quoted_message_id: impl Into<String>,
        quoted_sender_name: impl Into<String>,
        quoted_preview: impl Into<String>,
    ) -> Self {
        Self::microsoft365(
            Some(ProviderResourceRef::microsoft365(provider_id)),
            sender_name.into(),
            sent_at.into(),
            plain_text.into(),
            sent_by_current_user,
            false,
            importance,
            deleted,
            Vec::new(),
            vec![RemoteChatMessageQuote::microsoft365(
                ProviderResourceRef::microsoft365(quoted_message_id),
                quoted_sender_name.into(),
                quoted_preview.into(),
            )],
        )
    }

    /// Builds a message fixture quoting several source messages, given as
    /// `(provider id, sender, preview)` triples.
    #[cfg(test)]
    pub(crate) fn for_test_with_quote_references(
        provider_id: impl Into<String>,
        plain_text: impl Into<String>,
        sent_by_current_user: bool,
        quoted: &[(&str, &str, &str)],
    ) -> Self {
        Self::microsoft365(
            Some(ProviderResourceRef::microsoft365(provider_id)),
            "Sender".into(),
            "10:00".into(),
            plain_text.into(),
            sent_by_current_user,
            false,
            RemoteChatMessageImportance::Normal,
            false,
            Vec::new(),
            quoted
                .iter()
                .map(|(id, name, preview)| {
                    RemoteChatMessageQuote::microsoft365(
                        ProviderResourceRef::microsoft365(*id),
                        (*name).into(),
                        (*preview).into(),
                    )
                })
                .collect(),
        )
    }
}

/// A selection-scoped, provider-normalized message history for one Microsoft
/// 365 chat.
///
/// The opaque chat target lets the feature discard a late result after the
/// user selects another conversation, without comparing display labels or
/// exposing Graph paths, cursors, or raw DTOs outside the integrations seam.
/// The bounded history is session-only and never implies read receipts,
/// attachments, or other mutations. A message row may separately carry an
/// opaque target for its typed reaction actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChatMessagesProjection {
    chat: ProviderResourceRef,
    availability: RemoteChatMessagesAvailability,
    messages: Vec<RemoteChatMessage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChatMessagesAvailability {
    Available,
    AccessDenied,
    /// A transient read failure (network, throttle, or malformed response).
    /// It carries no rows and lets the feature return a Loading state to a
    /// retryable one.
    Unavailable,
}

impl RemoteChatMessagesProjection {
    /// Constructs a bounded message history only inside a provider adapter
    /// after it has validated the selected provider-owned chat target.
    pub(in crate::integrations) fn microsoft365(
        chat: ProviderResourceRef,
        messages: Vec<RemoteChatMessage>,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemoteChatMessagesAvailability::Available,
            messages,
        })
    }

    /// Replaces a selected chat's old history when Graph explicitly denies
    /// access. A 403 may be delegated consent or chat membership, so the
    /// normalized state does not speculate about its exact cause.
    pub(in crate::integrations) fn access_denied(
        chat: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemoteChatMessagesAvailability::AccessDenied,
            messages: Vec::new(),
        })
    }

    /// A target-bound marker that a selected read failed transiently. The
    /// app transport layer emits it so Loading state can become retryable;
    /// it never carries rows or a provider error.
    pub(crate) fn unavailable(chat: ProviderResourceRef) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemoteChatMessagesAvailability::Unavailable,
            messages: Vec::new(),
        })
    }

    pub(crate) const fn is_unavailable(&self) -> bool {
        matches!(
            self.availability,
            RemoteChatMessagesAvailability::Unavailable
        )
    }

    /// Returns the opaque selected-chat target. It can be compared or handed
    /// back to the transport seam, but cannot expose a provider identifier.
    pub(crate) fn chat(&self) -> ProviderResourceRef {
        self.chat.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemoteChatMessagesAvailability::AccessDenied
        )
    }

    pub(crate) fn messages(&self) -> &[RemoteChatMessage] {
        &self.messages
    }

    /// Builds a normalized selected-chat history for lifecycle tests without
    /// widening provider-owned identifier construction in production code.
    #[cfg(test)]
    pub(crate) fn for_test(
        chat_provider_id: impl Into<String>,
        messages: Vec<RemoteChatMessage>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(chat_provider_id),
            messages,
        )
        .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }

    /// Builds an explicit access-denied selected-chat projection for feature
    /// lifecycle tests without exposing that provider-only transition to
    /// production callers.
    #[cfg(test)]
    pub(crate) fn access_denied_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::access_denied(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }

    #[cfg(test)]
    pub(crate) fn unavailable_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::unavailable(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }
}

/// A selection-scoped, provider-normalized pinned-message view for one
/// Microsoft 365 chat.
///
/// Pinned-message data is a separate read from the ordinary selected history:
/// it remains session-only, carries the same opaque chat target for stale
/// result rejection, and grants no pin/unpin or generic message-mutation
/// capability to callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemotePinnedChatMessagesProjection {
    chat: ProviderResourceRef,
    availability: RemotePinnedChatMessagesAvailability,
    messages: Vec<RemoteChatMessage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemotePinnedChatMessagesAvailability {
    Available,
    AccessDenied,
    /// A transient read failure (network, throttle, or malformed response).
    /// It carries no rows and lets the feature return a Loading state to a
    /// retryable one.
    Unavailable,
}

/// The pinned-message adapter reads one bounded page only. Retain this cap at
/// the projection seam as well so future adapters cannot accidentally expand
/// a session-only panel into an unbounded history cache.
pub(in crate::integrations) const MAX_REMOTE_PINNED_CHAT_MESSAGES: usize = 50;

impl RemotePinnedChatMessagesProjection {
    /// Constructs a bounded pinned-message view only inside a provider adapter
    /// after it has validated the selected provider-owned chat target.
    pub(in crate::integrations) fn microsoft365(
        chat: ProviderResourceRef,
        messages: Vec<RemoteChatMessage>,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        if messages.len() > MAX_REMOTE_PINNED_CHAT_MESSAGES {
            return Err(TransportError::Unavailable(
                "Microsoft Graph returned too many pinned messages".into(),
            ));
        }
        Ok(Self {
            chat,
            availability: RemotePinnedChatMessagesAvailability::Available,
            messages,
        })
    }

    /// Replaces a selected chat's stale pinned-message view when Graph
    /// explicitly denies access. The chat directory and ordinary selected
    /// history remain separate surfaces with their own authorization states.
    pub(in crate::integrations) fn access_denied(
        chat: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemotePinnedChatMessagesAvailability::AccessDenied,
            messages: Vec::new(),
        })
    }

    /// A target-bound marker that a selected read failed transiently. The
    /// app transport layer emits it so Loading state can become retryable;
    /// it never carries rows or a provider error.
    pub(crate) fn unavailable(chat: ProviderResourceRef) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemotePinnedChatMessagesAvailability::Unavailable,
            messages: Vec::new(),
        })
    }

    pub(crate) const fn is_unavailable(&self) -> bool {
        matches!(
            self.availability,
            RemotePinnedChatMessagesAvailability::Unavailable
        )
    }

    /// Returns the opaque selected-chat target. It can only be compared or
    /// carried back to the transport seam; the provider identifier remains
    /// unavailable to feature and UI code.
    pub(crate) fn chat(&self) -> ProviderResourceRef {
        self.chat.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemotePinnedChatMessagesAvailability::AccessDenied
        )
    }

    /// Returns bounded, already-normalized presentation rows. The feature
    /// must render these read-only and may not turn their opaque nested targets
    /// into a pin or message mutation route.
    pub(crate) fn messages(&self) -> &[RemoteChatMessage] {
        &self.messages
    }

    /// Builds a normalized pinned-message view for lifecycle tests without
    /// widening provider-owned identifier construction in production code.
    #[cfg(test)]
    pub(crate) fn for_test(
        chat_provider_id: impl Into<String>,
        messages: Vec<RemoteChatMessage>,
    ) -> Self {
        Self::microsoft365(
            ProviderResourceRef::microsoft365(chat_provider_id),
            messages,
        )
        .expect("test pinned-message targets must be valid and bounded")
    }

    /// Builds an explicit access-denied pinned-message projection for feature
    /// lifecycle tests without exposing that provider-only transition to
    /// production callers.
    #[cfg(test)]
    pub(crate) fn access_denied_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::access_denied(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test pinned-message targets must be valid Microsoft 365 identifiers")
    }

    #[cfg(test)]
    pub(crate) fn unavailable_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::unavailable(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }
}

/// One presentation-safe member of a selected Microsoft 365 chat.
///
/// This deliberately has no provider ID, email, tenant, or mutation route.
/// The feature can only render a normalized display name and one of the two
/// role badges this client recognizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChatMember {
    display_name: String,
    role: Option<RemoteChatMemberRole>,
    presence: Option<RemoteMemberPresence>,
    mention: Option<MentionHandle>,
}

/// The most mentions one message may carry.
pub(crate) const MAX_MENTIONS_PER_MESSAGE: usize = 20;

/// An opaque, session-only reference to one member of one chat, issued by the
/// adapter from an explicit People roster read. It stands in for the member's
/// user ID, which never leaves the adapter: the composer holds a handle, and
/// the adapter resolves it back (within the chat it was issued for) when a
/// mention is sent. `Debug` reveals nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct MentionHandle(u64);

impl MentionHandle {
    pub(in crate::integrations) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Test-only: a handle with a chosen value, for feature fixtures.
    #[cfg(test)]
    pub(crate) const fn for_test(raw: u64) -> Self {
        Self(raw)
    }

    pub(in crate::integrations) const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for MentionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MentionHandle(..)")
    }
}

/// The normalized availability categories this client presents. Graph's
/// `availability` and `activity` strings are folded into these and anything
/// unrecognized fails closed to no presence at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteMemberPresence {
    Available,
    Busy,
    DoNotDisturb,
    Away,
    BeRightBack,
    Offline,
}

impl RemoteMemberPresence {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Busy => "Busy",
            Self::DoNotDisturb => "Do not disturb",
            Self::Away => "Away",
            Self::BeRightBack => "Be right back",
            Self::Offline => "Offline",
        }
    }
}

/// Why presence is or is not shown for a roster. It carries no provider data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PresenceStatus {
    Available,
    NotGranted,
    UnsupportedInCloud,
    Throttled,
    #[default]
    Unavailable,
}

impl PresenceStatus {
    /// Whether the feature may keep scheduling a presence refresh.
    pub(crate) const fn keeps_refreshing(self) -> bool {
        matches!(self, Self::Available | Self::Throttled | Self::Unavailable)
    }
}

/// The only member roles this client presents. Unknown Graph role values stay
/// absent instead of turning a future provider string into UI authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::integrations) enum RemoteChatMemberRole {
    Owner,
    Guest,
}

impl RemoteChatMemberRole {
    const fn label(self) -> &'static str {
        match self {
            Self::Owner => "Owner",
            Self::Guest => "Guest",
        }
    }
}

impl RemoteChatMember {
    /// Constructs a safe row only after the provider adapter has bounded and
    /// normalized its display metadata. There is intentionally no target or
    /// identifier parameter: the People surface is read-only.
    pub(in crate::integrations) fn microsoft365(
        display_name: String,
        role: Option<RemoteChatMemberRole>,
    ) -> Self {
        Self {
            display_name,
            role,
            presence: None,
            mention: None,
        }
    }

    /// Test-only: a roster row that can be mentioned.
    #[cfg(test)]
    pub(crate) fn with_mention_for_test(mut self, handle: MentionHandle) -> Self {
        self.mention = Some(handle);
        self
    }

    pub(in crate::integrations) fn set_mention(&mut self, mention: Option<MentionHandle>) {
        self.mention = mention;
    }

    /// The handle to mention this member with, when they can be mentioned.
    pub(crate) const fn mention(&self) -> Option<MentionHandle> {
        self.mention
    }

    pub(in crate::integrations) fn set_presence(&mut self, presence: Option<RemoteMemberPresence>) {
        self.presence = presence;
    }

    pub(crate) const fn presence(&self) -> Option<RemoteMemberPresence> {
        self.presence
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn role_label(&self) -> Option<&str> {
        self.role.map(RemoteChatMemberRole::label)
    }

    #[cfg(test)]
    pub(crate) fn with_presence_for_test(mut self, presence: RemoteMemberPresence) -> Self {
        self.presence = Some(presence);
        self
    }

    /// Builds a presentation-only member row for feature lifecycle tests.
    /// Production code may receive role state only from the integration
    /// adapter; unknown labels deliberately remain absent.
    #[cfg(test)]
    pub(crate) fn for_test(display_name: impl Into<String>, role_label: Option<&str>) -> Self {
        let role = match role_label {
            Some("Owner") => Some(RemoteChatMemberRole::Owner),
            Some("Guest") => Some(RemoteChatMemberRole::Guest),
            None | Some(_) => None,
        };
        Self::microsoft365(display_name.into(), role)
    }
}

/// A selected-chat, provider-normalized member roster.
///
/// The roster is session-only and target-bound. It is independent from
/// ordinary chat history and Pinned messages so opening or closing People
/// never creates a read receipt, caches member IDs, or grants membership
/// management capabilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChatMembersProjection {
    chat: ProviderResourceRef,
    availability: RemoteChatMembersAvailability,
    members: Vec<RemoteChatMember>,
    presence_status: PresenceStatus,
    /// Only for a presence-only update: one entry per roster row, in order.
    presence_updates: Vec<Option<RemoteMemberPresence>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChatMembersAvailability {
    Available,
    AccessDenied,
    /// A presence-only refresh that never carries rows.
    PresenceUpdate,
    /// A transient read failure (network, throttle, or malformed response).
    /// It carries no rows and lets the feature return a Loading state to a
    /// retryable one.
    Unavailable,
}

/// One explicit People-panel request may retain no more than this many safe
/// presentation rows. The integration adapter is also responsible for
/// bounding candidate JSON before it reaches this seam.
pub(in crate::integrations) const MAX_REMOTE_CHAT_MEMBERS: usize = 100;

impl RemoteChatMembersProjection {
    /// Constructs a bounded roster only inside a provider adapter after it
    /// has validated the selected Microsoft 365 chat target.
    pub(in crate::integrations) fn microsoft365(
        chat: ProviderResourceRef,
        members: Vec<RemoteChatMember>,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        if members.len() > MAX_REMOTE_CHAT_MEMBERS {
            return Err(TransportError::Unavailable(
                "Microsoft Graph returned too many chat members".into(),
            ));
        }
        Ok(Self {
            chat,
            availability: RemoteChatMembersAvailability::Available,
            members,
            presence_status: PresenceStatus::Unavailable,
            presence_updates: Vec::new(),
        })
    }

    pub(in crate::integrations) fn with_presence_status(mut self, status: PresenceStatus) -> Self {
        self.presence_status = status;
        self
    }

    /// A presence-only refresh for an already-open roster. It replaces no
    /// rows: `updates` is positional and applies only if it matches.
    pub(in crate::integrations) fn presence_update(
        chat: ProviderResourceRef,
        updates: Vec<Option<RemoteMemberPresence>>,
        status: PresenceStatus,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        if updates.len() > MAX_REMOTE_CHAT_MEMBERS {
            return Err(TransportError::Unavailable(
                "Microsoft Graph returned too many presence rows".into(),
            ));
        }
        Ok(Self {
            chat,
            availability: RemoteChatMembersAvailability::PresenceUpdate,
            members: Vec::new(),
            presence_status: status,
            presence_updates: updates,
        })
    }

    /// The same marker for a refresh that failed; labels are kept.
    pub(crate) fn presence_failed(chat: ProviderResourceRef) -> Result<Self, TransportError> {
        Self::presence_update(chat, Vec::new(), PresenceStatus::Unavailable)
    }

    pub(crate) const fn is_presence_update(&self) -> bool {
        matches!(
            self.availability,
            RemoteChatMembersAvailability::PresenceUpdate
        )
    }

    pub(crate) const fn presence_status(&self) -> PresenceStatus {
        self.presence_status
    }

    pub(crate) fn presence_updates(&self) -> &[Option<RemoteMemberPresence>] {
        &self.presence_updates
    }

    /// Replaces stale People-panel rows when Graph explicitly denies access.
    /// Other selected-chat views retain their own authorization states.
    pub(in crate::integrations) fn access_denied(
        chat: ProviderResourceRef,
    ) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemoteChatMembersAvailability::AccessDenied,
            members: Vec::new(),
            presence_status: PresenceStatus::Unavailable,
            presence_updates: Vec::new(),
        })
    }

    /// A target-bound marker that a selected read failed transiently. The
    /// app transport layer emits it so Loading state can become retryable;
    /// it never carries rows or a provider error.
    pub(crate) fn unavailable(chat: ProviderResourceRef) -> Result<Self, TransportError> {
        validate_remote_chat_selection(&chat)?;
        Ok(Self {
            chat,
            availability: RemoteChatMembersAvailability::Unavailable,
            members: Vec::new(),
            presence_status: PresenceStatus::Unavailable,
            presence_updates: Vec::new(),
        })
    }

    pub(crate) const fn is_unavailable(&self) -> bool {
        matches!(
            self.availability,
            RemoteChatMembersAvailability::Unavailable
        )
    }

    /// Returns only an opaque selected-chat target for stale-result checks.
    /// It cannot expose a Graph identifier outside the integration seam.
    pub(crate) fn chat(&self) -> ProviderResourceRef {
        self.chat.clone()
    }

    pub(crate) const fn is_access_denied(&self) -> bool {
        matches!(
            self.availability,
            RemoteChatMembersAvailability::AccessDenied
        )
    }

    /// Returns bounded, safe rows without provider IDs, emails, or a member
    /// action target.
    pub(crate) fn members(&self) -> &[RemoteChatMember] {
        &self.members
    }

    /// Builds a bounded selected-chat roster for lifecycle tests without
    /// widening provider-owned target construction in production builds.
    #[cfg(test)]
    pub(crate) fn for_test(
        chat_provider_id: impl Into<String>,
        members: Vec<RemoteChatMember>,
    ) -> Self {
        Self::microsoft365(ProviderResourceRef::microsoft365(chat_provider_id), members)
            .expect("test selected-chat targets and member bounds must be valid")
    }

    /// Builds an explicit denied roster for feature lifecycle tests without
    /// exposing a production constructor outside integrations.
    #[cfg(test)]
    pub(crate) fn access_denied_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::access_denied(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }

    /// A presence-only refresh for feature lifecycle tests.
    #[cfg(test)]
    pub(crate) fn presence_update_for_test(
        chat_provider_id: impl Into<String>,
        updates: Vec<Option<RemoteMemberPresence>>,
        status: PresenceStatus,
    ) -> Self {
        Self::presence_update(
            ProviderResourceRef::microsoft365(chat_provider_id),
            updates,
            status,
        )
        .expect("test presence updates must be valid")
    }

    /// Sets the roster's presence status for feature lifecycle tests.
    #[cfg(test)]
    pub(crate) fn with_status_for_test(self, status: PresenceStatus) -> Self {
        self.with_presence_status(status)
    }

    #[cfg(test)]
    pub(crate) fn unavailable_for_test(chat_provider_id: impl Into<String>) -> Self {
        Self::unavailable(ProviderResourceRef::microsoft365(chat_provider_id))
            .expect("test selected-chat targets must be valid Microsoft 365 identifiers")
    }
}

fn validate_remote_chat_selection(chat: &ProviderResourceRef) -> Result<(), TransportError> {
    chat.require_microsoft365("chat")?;
    chat.validate("chat id")?;
    Ok(())
}
