//! Ephemeral Microsoft 365 Teams and channels directory.
//!
//! This is intentionally separate from the bundled Teams catalog and its
//! local persistence projection. The integration layer supplies normalized,
//! opaque provider targets; this module retains them only for the current
//! session and renders a deliberately constrained directory. A selected channel
//! can additionally receive bounded root-post, selected-root-post reply, and
//! channel-file metadata projections, plus narrowly modeled text-only
//! root-post/reply commands and explicit bounded-palette reactions on visible
//! root posts and direct replies.
//! File content operations, meetings, edits, nested replies, and all other
//! mutations still require their own provider contracts and cannot accidentally
//! reuse local-preview actions.

pub(super) use super::remote_delivery::RemoteChannelDeliveryError as RemoteChannelPostError;
use super::remote_delivery::{
    RemoteChannelDeliveryError, RemoteChannelDeliveryState, RemoteChannelDeliveryTarget,
    RemoteChannelPostSendRequest, RemoteChannelPostSubmissionId, RemoteChannelReplySendRequest,
    RemoteChannelReplySubmissionId,
};
use crate::integrations::{
    ProviderBrowserUrl, ProviderResourceRef, RemoteMessageReaction, WorkspaceIntentId,
};
use crate::shared::reaction::ReactionKind;

mod render;

pub(super) use self::render::{RemoteChannelPageAction, draw_directory, draw_selected_channel};

/// A normalized Microsoft 365 directory supplied by an integration adapter.
///
/// Constructors accept only opaque resource references. Production callers
/// must obtain those references from an integration projection rather than a
/// display label or a local catalog key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteTeamsDirectory {
    teams: Vec<RemoteTeamDirectory>,
}

impl RemoteTeamsDirectory {
    pub(crate) fn microsoft365(teams: Vec<RemoteTeamDirectory>) -> Self {
        Self { teams }
    }
}

/// One presentation-safe Microsoft 365 team and its bounded channel list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteTeamDirectory {
    target: ProviderResourceRef,
    display_name: String,
    description: String,
    channels: Vec<RemoteChannelDirectory>,
}

impl RemoteTeamDirectory {
    /// Builds a directory row from an integration-normalized target and text.
    /// The caller is responsible for ensuring `target` is provider-backed;
    /// feature code intentionally has no API to construct Graph identifiers.
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        display_name: String,
        description: String,
        channels: Vec<RemoteChannelDirectory>,
    ) -> Self {
        Self {
            target,
            display_name,
            description,
            channels,
        }
    }
}

/// One presentation-safe Microsoft 365 channel. It deliberately contains no
/// messages: a message feed needs a selection-scoped permission and fetch
/// policy rather than an unbounded directory refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelDirectory {
    target: ProviderResourceRef,
    display_name: String,
    description: String,
    membership_label: String,
    archived: bool,
}

impl RemoteChannelDirectory {
    /// Builds a read-only channel row from integration-normalized data.
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        display_name: String,
        description: String,
        membership_label: String,
        archived: bool,
    ) -> Self {
        Self {
            target,
            display_name,
            description,
            membership_label,
            archived,
        }
    }
}

/// A selected channel's session-only root-post projection.
///
/// The provider adapter has already bounded and normalized this data. Keeping
/// the requested team and channel targets alongside it makes a late response
/// harmless: the feature applies it only while the same opaque selection is
/// still active. Nothing here is included in `StoredTeams`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelMessagesDirectory {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    availability: RemoteChannelMessagesAvailability,
    posts: Vec<RemoteChannelPost>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelMessagesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelMessagesDirectory {
    pub(crate) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        posts: Vec<RemoteChannelPost>,
    ) -> Self {
        Self {
            team,
            channel,
            availability: RemoteChannelMessagesAvailability::Available,
            posts,
        }
    }

    /// Replaces a selected channel's prior feed when Microsoft 365 denies the
    /// read. This can be caused by delegated consent, channel membership, or
    /// another provider-side authorization rule, so presentation must not
    /// claim that one specific remediation will always work.
    pub(crate) fn access_denied(team: ProviderResourceRef, channel: ProviderResourceRef) -> Self {
        Self {
            team,
            channel,
            availability: RemoteChannelMessagesAvailability::AccessDenied,
            posts: Vec::new(),
        }
    }
}

/// One presentation-safe root post from Microsoft 365.
///
/// `target` is deliberately retained as an opaque provider identity for the
/// selected-post reply read. It must never be recreated from a title or
/// copied into a local discussion record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelPost {
    target: ProviderResourceRef,
    author: String,
    created_at: String,
    subject: String,
    plain_text: String,
    deleted: bool,
    edited: bool,
    /// Bounded provider-projected reaction summaries. This is never a local
    /// optimistic overlay; the next selected-channel projection is the sole
    /// authority for visible counts and current-user selections.
    reactions: Vec<RemoteMessageReaction>,
}

impl RemoteChannelPost {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        author: String,
        created_at: String,
        subject: String,
        plain_text: String,
        deleted: bool,
        edited: bool,
        reactions: Vec<RemoteMessageReaction>,
    ) -> Self {
        Self {
            target,
            author,
            created_at,
            subject,
            plain_text,
            deleted,
            edited,
            reactions,
        }
    }

    fn reaction(&self, kind: ReactionKind) -> Option<RemoteMessageReaction> {
        self.reactions
            .iter()
            .copied()
            .find(|reaction| reaction.kind() == kind)
    }

    fn reaction_is_selected(&self, kind: ReactionKind) -> bool {
        self.reaction(kind)
            .is_some_and(|reaction| reaction.reacted_by_current_user())
    }
}

/// A selected root post's session-only reply projection.
///
/// Every level of the route stays explicit and opaque. A reply response can
/// therefore be discarded if the user changed either the team, channel, or
/// root post while the provider read was in flight. Nothing in this type is
/// included in `StoredTeams`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelRepliesDirectory {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    availability: RemoteChannelRepliesAvailability,
    replies: Vec<RemoteChannelReply>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelRepliesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelRepliesDirectory {
    /// Builds a normalized, bounded reply projection read for one root post.
    /// A separate text-only command may reply to that same root; all targets
    /// still originate in the integration layer.
    pub(crate) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        replies: Vec<RemoteChannelReply>,
    ) -> Self {
        Self {
            team,
            channel,
            post,
            availability: RemoteChannelRepliesAvailability::Available,
            replies,
        }
    }

    /// Marks one exact selected root post unreadable without making a claim
    /// about which provider-side authorization condition caused the denial.
    pub(crate) fn access_denied(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> Self {
        Self {
            team,
            channel,
            post,
            availability: RemoteChannelRepliesAvailability::AccessDenied,
            replies: Vec::new(),
        }
    }
}

/// One presentation-safe Microsoft 365 reply.
///
/// Its opaque target may be used only for a bounded-palette action scoped to
/// the selected team, channel, and parent root post. Editing, deleting, and
/// nested replying remain unavailable in this feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelReply {
    target: ProviderResourceRef,
    author: String,
    created_at: String,
    plain_text: String,
    deleted: bool,
    edited: bool,
    reactions: Vec<RemoteMessageReaction>,
}

impl RemoteChannelReply {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        author: String,
        created_at: String,
        plain_text: String,
        deleted: bool,
        edited: bool,
        reactions: Vec<RemoteMessageReaction>,
    ) -> Self {
        Self {
            target,
            author,
            created_at,
            plain_text,
            deleted,
            edited,
            reactions,
        }
    }

    fn reaction(&self, kind: ReactionKind) -> Option<RemoteMessageReaction> {
        self.reactions
            .iter()
            .copied()
            .find(|reaction| reaction.kind() == kind)
    }

    fn reaction_is_selected(&self, kind: ReactionKind) -> bool {
        self.reaction(kind)
            .is_some_and(|reaction| reaction.reacted_by_current_user())
    }
}

/// A selected channel's session-only file metadata projection.
///
/// The provider adapter resolves the backing SharePoint folder and normalizes
/// only its direct children before this boundary. The opaque team and channel
/// targets let this feature reject a late folder read after navigation, while
/// neither a drive route, folder ID, content URL, nor local path can reach
/// feature state or durable Teams storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelFilesDirectory {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    availability: RemoteChannelFilesAvailability,
    files: Vec<RemoteChannelFile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteChannelFilesAvailability {
    Available,
    AccessDenied,
}

impl RemoteChannelFilesDirectory {
    /// Builds a bounded, presentation-safe channel-folder listing after the
    /// integration adapter has resolved and normalized its provider route.
    pub(crate) fn microsoft365(
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        files: Vec<RemoteChannelFile>,
    ) -> Self {
        Self {
            team,
            channel,
            availability: RemoteChannelFilesAvailability::Available,
            files,
        }
    }

    /// Represents a provider refusal for one selected channel without
    /// guessing whether delegated consent, membership, or its backing store
    /// caused it.
    pub(crate) fn access_denied(team: ProviderResourceRef, channel: ProviderResourceRef) -> Self {
        Self {
            team,
            channel,
            availability: RemoteChannelFilesAvailability::AccessDenied,
            files: Vec::new(),
        }
    }
}

/// One presentation-safe item from a selected channel's backing folder.
///
/// The opaque target is deliberately retained for an explicit browser handoff
/// when Microsoft 365 supplies a validated destination. The current view
/// offers no preview, browse, download, upload, sharing, rename, move, or
/// delete capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteChannelFile {
    target: ProviderResourceRef,
    name: String,
    modified_at: String,
    size_bytes: Option<u64>,
    kind_label: String,
    web_url: Option<ProviderBrowserUrl>,
}

impl RemoteChannelFile {
    pub(crate) fn microsoft365(
        target: ProviderResourceRef,
        name: String,
        modified_at: String,
        size_bytes: Option<u64>,
        kind_label: String,
        web_url: Option<ProviderBrowserUrl>,
    ) -> Self {
        Self {
            target,
            name,
            modified_at,
            size_bytes,
            kind_label,
            web_url,
        }
    }
}

/// Session-only selection and expansion state for the remote hierarchy.
///
/// All identity comparisons are by opaque provider target, never display
/// name. This makes a reordered response or duplicate name harmless and
/// ensures nothing is written to `StoredTeams`.
#[derive(Default)]
pub(super) struct RemoteTeamsState {
    teams: Vec<RemoteTeamDirectory>,
    expanded_teams: Vec<ProviderResourceRef>,
    selection: Option<RemoteChannelSelection>,
    messages: RemoteChannelMessagesState,
    reply_selection: Option<RemoteChannelPostSelection>,
    replies: RemoteChannelRepliesState,
    surface: RemoteChannelSurface,
    files: RemoteChannelFilesState,
    /// One global remote-mutation gate for root posts and selected-root-post
    /// replies. Idle drafts stay bound to their opaque targets, while active
    /// delivery recovery remains attached to the original request.
    delivery: RemoteChannelDeliveryState,
    /// Remote root-post reactions deliberately have no optimistic feed
    /// projection. Each target stays locked until a matching provider feed
    /// replaces it, preventing repeated clicks from issuing duplicate writes.
    pending_post_reactions: Vec<PendingRemoteChannelPostReaction>,
    /// Reply reactions use the same projection-owned lifecycle as root-post
    /// reactions, but retain the complete team/channel/root-post/reply route
    /// so a reply from another thread cannot release this exact gate.
    pending_reply_reactions: Vec<PendingRemoteChannelReplyReaction>,
    /// A feature-owned one-shot lifecycle signal. The workspace transport
    /// drains it when it must cancel a queued or active reply read after the
    /// corresponding selected-post projection has been discarded.
    replies_clear_requested: bool,
    /// Mirrors `replies_clear_requested`, but is intentionally separate so
    /// changing a channel can cancel a stale folder read without affecting a
    /// root-post reply poll (and vice versa).
    files_clear_requested: bool,
    /// A direct click can queue one validated channel-file browser handoff.
    /// These opaque values are never part of a local Files state, a transport
    /// request, or a durable Teams snapshot.
    pending_channel_file_open_links: Vec<ProviderBrowserUrl>,
    initialized: bool,
}

#[derive(Clone)]
struct RemoteChannelSelection {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
}

#[derive(Clone)]
struct RemoteChannelPostSelection {
    post: ProviderResourceRef,
}

/// One exact remote root-post reaction request. This remains feature-local
/// until the Teams facade turns it into a provider-neutral action; neither a
/// local discussion nor durable reaction state is changed.
pub(super) struct RemoteChannelPostReactionRequest {
    pub(super) team: ProviderResourceRef,
    pub(super) channel: ProviderResourceRef,
    pub(super) post: ProviderResourceRef,
    pub(super) reaction: ReactionKind,
    pub(super) selected: bool,
}

/// The provider route whose reaction click is waiting for a matching root-post
/// projection. Keeping every segment avoids unblocking a same-ID post from a
/// different selected channel.
struct PendingRemoteChannelPostReaction {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    /// Different kinds on the same post may be in flight independently.
    /// Keeping this typed makes their intent binding and reconciliation exact.
    reaction: ReactionKind,
    /// The provider-owned selection value the accepted or ambiguous mutation
    /// must eventually project. A same-post response can still be stale.
    selected: bool,
    intent_id: Option<WorkspaceIntentId>,
}

/// One exact remote reply reaction request. It cannot be retargeted to a
/// local thread or another root post before the Teams facade emits it.
pub(super) struct RemoteChannelReplyReactionRequest {
    pub(super) team: ProviderResourceRef,
    pub(super) channel: ProviderResourceRef,
    pub(super) post: ProviderResourceRef,
    pub(super) reply: ProviderResourceRef,
    pub(super) reaction: ReactionKind,
    pub(super) selected: bool,
}

/// The complete provider route whose reply reaction awaits reconciliation.
/// A successful or uncertain write remains gated until a matching selected
/// reply projection supplies the provider-owned count and selection.
struct PendingRemoteChannelReplyReaction {
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    reply: ProviderResourceRef,
    /// Different kinds on the same reply may be in flight independently.
    reaction: ReactionKind,
    /// The provider-owned selection value the accepted or ambiguous mutation
    /// must eventually project. Merely seeing the same reply again can be a
    /// stale read, so it is not enough to release this gate.
    selected: bool,
    intent_id: Option<WorkspaceIntentId>,
}

#[derive(Default)]
enum RemoteChannelMessagesState {
    #[default]
    None,
    Loading,
    AccessDenied,
    Ready(Vec<RemoteChannelPost>),
}

#[derive(Default)]
enum RemoteChannelRepliesState {
    #[default]
    None,
    Loading,
    AccessDenied,
    Ready(Vec<RemoteChannelReply>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RemoteChannelSurface {
    #[default]
    Posts,
    Files,
}

#[derive(Default)]
enum RemoteChannelFilesState {
    #[default]
    None,
    Loading,
    AccessDenied,
    Ready(Vec<RemoteChannelFile>),
}

impl RemoteTeamsState {
    pub(super) fn replace(&mut self, directory: RemoteTeamsDirectory) {
        let previous_expanded = std::mem::take(&mut self.expanded_teams);
        let had_projection = self.initialized;
        self.teams = directory.teams;
        self.initialized = true;

        self.expanded_teams = self
            .teams
            .iter()
            .filter(|team| !had_projection || previous_expanded.contains(&team.target))
            .map(|team| team.target.clone())
            .collect();

        if !self.selection_is_available() {
            self.selection = None;
            self.messages = RemoteChannelMessagesState::None;
            // A directory snapshot can temporarily omit an otherwise
            // selected channel. Its rows are no longer safe to render, but a
            // controller-bound non-idempotent reaction may still resolve
            // after any previously visited route reappears. Preserve every
            // such bound route as a tombstone; an unbound click is abandoned.
            self.clear_replies_preserving_bound_reactions();
            self.retain_bound_post_reactions();
            self.clear_files();
            self.stash_idle_remote_delivery();
            self.surface = RemoteChannelSurface::Posts;
        } else if let Some((team, channel)) = self.selected_targets() {
            let channel_is_archived = self.selected().is_some_and(|(_, channel)| channel.archived);
            if channel_is_archived {
                // A directory update can archive the still-selected channel
                // after a UI click but before its coordinator handoff drains.
                // Its unbound clicks can no longer be dispatched; retain only
                // controller-owned writes whose delivery is still ambiguous.
                self.discard_unbound_reactions_for_channel(&team, &channel);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.clear_projection();
        // An explicit disconnect/profile boundary discards every session-only
        // remote draft and outcome. Its transport controller invalidates any
        // queued command at the same boundary, so no post can outlive this
        // state.
        self.delivery.clear();
    }

    /// Clears a lost remote projection while retaining session-only composer
    /// state for an outcome that is already owned by the transport controller.
    /// This is deliberately distinct from an explicit disconnect/profile
    /// reset: network or worker loss cannot prove that a POST did not reach
    /// Microsoft Graph, so the user text must remain available for the later
    /// delivery-uncertain mapping. The same ambiguity applies to
    /// controller-bound reactions, which remain route-specific tombstones
    /// until a later matching projection reconciles them.
    pub(super) fn clear_for_transport_loss(&mut self) {
        self.clear_projection_preserving_bound_reactions();
    }

    fn clear_projection(&mut self) {
        self.teams.clear();
        self.expanded_teams.clear();
        self.selection = None;
        self.messages = RemoteChannelMessagesState::None;
        self.clear_replies();
        self.clear_files();
        self.clear_pending_post_reactions();
        self.surface = RemoteChannelSurface::Posts;
        self.initialized = false;
    }

    /// Clears visible provider state after a transient transport failure while
    /// retaining only controller-owned reaction gates. The next directory and
    /// selection projection can safely reconcile those gates by their full
    /// opaque route; no unbound click may escape the failed transport.
    fn clear_projection_preserving_bound_reactions(&mut self) {
        self.teams.clear();
        self.expanded_teams.clear();
        self.selection = None;
        self.messages = RemoteChannelMessagesState::None;
        self.clear_replies_preserving_bound_reactions();
        self.clear_files();
        self.retain_bound_post_reactions();
        self.surface = RemoteChannelSurface::Posts;
        self.initialized = false;
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.teams.is_empty()
    }

    pub(super) const fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.messages = RemoteChannelMessagesState::None;
        self.clear_replies_preserving_bound_reactions();
        self.clear_files();
        self.retain_bound_post_reactions();
        self.stash_idle_remote_delivery();
        self.surface = RemoteChannelSurface::Posts;
    }

    /// Applies a response only if it still belongs to the current opaque
    /// selection. A remote worker can finish after the user has selected a
    /// different channel; in that case its posts must remain invisible.
    pub(super) fn replace_messages(&mut self, directory: RemoteChannelMessagesDirectory) -> bool {
        if !self.selected_channel_is(&directory.team, &directory.channel) {
            return false;
        }
        self.resolve_pending_post_reactions(&directory);
        let selected_post_is_still_visible = match directory.availability {
            RemoteChannelMessagesAvailability::Available => {
                self.reply_selection.as_ref().is_none_or(|selection| {
                    directory
                        .posts
                        .iter()
                        .any(|post| post.target == selection.post)
                })
            }
            RemoteChannelMessagesAvailability::AccessDenied => self.reply_selection.is_none(),
        };
        if !selected_post_is_still_visible {
            self.clear_replies_preserving_bound_reactions();
        }
        self.messages = match directory.availability {
            RemoteChannelMessagesAvailability::Available => {
                RemoteChannelMessagesState::Ready(directory.posts)
            }
            RemoteChannelMessagesAvailability::AccessDenied => {
                RemoteChannelMessagesState::AccessDenied
            }
        };
        true
    }

    /// Discards the selected channel's transient post feed while keeping the
    /// directory selection intact. Bound non-idempotent reaction gates remain
    /// locked across this same-selection refresh; an unbound click is not yet
    /// owned by the controller and is safely discarded instead. The page
    /// truthfully returns to its loading state until the app supplies a
    /// matching replacement.
    pub(super) fn clear_messages(&mut self) {
        if self.selection.is_none() {
            self.clear_replies_preserving_bound_reactions();
            self.retain_bound_post_reactions();
            self.messages = RemoteChannelMessagesState::None;
            return;
        }

        self.clear_replies_for_same_channel_refresh();
        self.retain_bound_post_reactions();
        self.messages = RemoteChannelMessagesState::Loading;
    }

    /// Applies a reply response only while every opaque route segment still
    /// identifies the selected root post. A late response for a different
    /// post in the same channel is just as stale as one for another channel.
    pub(super) fn replace_replies(&mut self, directory: RemoteChannelRepliesDirectory) -> bool {
        if !self.selected_reply_is(&directory.team, &directory.channel, &directory.post) {
            return false;
        }
        self.resolve_pending_reply_reactions(&directory);
        self.replies = match directory.availability {
            RemoteChannelRepliesAvailability::Available => {
                RemoteChannelRepliesState::Ready(directory.replies)
            }
            RemoteChannelRepliesAvailability::AccessDenied => {
                RemoteChannelRepliesState::AccessDenied
            }
        };
        true
    }

    /// Applies a channel-folder response only while it still belongs to the
    /// selected opaque team and channel. A drive/folder resolution can finish
    /// after navigation; it must never populate another channel's view.
    pub(super) fn replace_files(&mut self, directory: RemoteChannelFilesDirectory) -> bool {
        if !self.selected_channel_is(&directory.team, &directory.channel) {
            return false;
        }
        // A replacement establishes the only current item target set. Do not
        // let a link clicked before it escape after that item disappeared or
        // changed ownership.
        self.pending_channel_file_open_links.clear();
        self.files = match directory.availability {
            RemoteChannelFilesAvailability::Available => {
                RemoteChannelFilesState::Ready(directory.files)
            }
            RemoteChannelFilesAvailability::AccessDenied => RemoteChannelFilesState::AccessDenied,
        };
        true
    }

    /// Stages one system-browser handoff only after a direct click on a file
    /// still contained in this selected channel's current Ready projection.
    /// The destination remains typed and opaque until the app shell performs
    /// its final HTTPS validation immediately before launch.
    pub(super) fn request_channel_file_open(&mut self, target: ProviderResourceRef) -> bool {
        let RemoteChannelFilesState::Ready(files) = &self.files else {
            return false;
        };
        let Some(web_url) = files
            .iter()
            .find(|file| file.target == target)
            .and_then(|file| file.web_url.clone())
        else {
            return false;
        };
        self.pending_channel_file_open_links.push(web_url);
        true
    }

    pub(super) fn take_channel_file_open_links(&mut self) -> Vec<ProviderBrowserUrl> {
        std::mem::take(&mut self.pending_channel_file_open_links)
    }

    /// Drains the request to cancel any selected-post reply read which no
    /// longer has a feature-owned target. This is deliberately a one-shot
    /// signal rather than a provider concern in the Teams feature.
    pub(super) fn take_replies_clear_requested(&mut self) -> bool {
        std::mem::take(&mut self.replies_clear_requested)
    }

    /// Drains the request to cancel a channel-files read which lost its
    /// selected channel. This is independent of selected-post reply work.
    pub(super) fn take_files_clear_requested(&mut self) -> bool {
        std::mem::take(&mut self.files_clear_requested)
    }

    fn selection_is_available(&self) -> bool {
        let Some(selection) = &self.selection else {
            return false;
        };
        self.teams.iter().any(|team| {
            team.target == selection.team
                && team
                    .channels
                    .iter()
                    .any(|channel| channel.target == selection.channel)
        })
    }

    fn is_expanded(&self, target: &ProviderResourceRef) -> bool {
        self.expanded_teams
            .iter()
            .any(|candidate| candidate == target)
    }

    fn toggle_expanded(&mut self, target: ProviderResourceRef) {
        if let Some(index) = self
            .expanded_teams
            .iter()
            .position(|candidate| *candidate == target)
        {
            self.expanded_teams.swap_remove(index);
        } else {
            self.expanded_teams.push(target);
        }
    }

    pub(super) fn select(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        let available = self.teams.iter().any(|candidate| {
            candidate.target == team
                && candidate
                    .channels
                    .iter()
                    .any(|candidate| candidate.target == channel)
        });
        if available {
            let changed = !self.selected_channel_is(&team, &channel);
            if changed {
                self.selection = Some(RemoteChannelSelection {
                    team: team.clone(),
                    channel: channel.clone(),
                });
                self.messages = RemoteChannelMessagesState::Loading;
                // A reaction command can remain queued or in flight after
                // ordinary navigation. Keep bound commands as opaque-route
                // tombstones so returning to their target cannot duplicate a
                // non-idempotent mutation; discard only unbound UI clicks.
                self.clear_replies_preserving_bound_reactions();
                self.retain_bound_post_reactions();
                self.clear_files();
                self.stash_idle_remote_delivery();
                self.surface = RemoteChannelSurface::Posts;
            }
            changed
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(super) fn select_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        self.select(team, channel)
    }

    fn selected(&self) -> Option<(&RemoteTeamDirectory, &RemoteChannelDirectory)> {
        let selection = self.selection.as_ref()?;
        let team = self
            .teams
            .iter()
            .find(|team| team.target == selection.team)?;
        let channel = team
            .channels
            .iter()
            .find(|channel| channel.target == selection.channel)?;
        Some((team, channel))
    }

    pub(super) fn selected_channel_is(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| selection.team == *team && selection.channel == *channel)
    }

    /// Reports whether this exact selected channel still owns a selected root
    /// post reply view. A coordinator uses it to distinguish a same-channel
    /// refresh (which must leave its reply read alone) from an initial or
    /// changed channel selection (which must cancel the obsolete read).
    pub(super) fn has_selected_reply_for(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) -> bool {
        self.selected_channel_is(team, channel) && self.reply_selection.is_some()
    }

    pub(super) fn selected_targets(&self) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        let selection = self.selection.as_ref()?;
        Some((selection.team.clone(), selection.channel.clone()))
    }

    /// Stages one bounded-palette reaction change for a visible root post
    /// without changing the rendered provider projection. The exact
    /// target-and-kind pair stays locked until that same reaction reports the
    /// requested provider-owned selection in a later matching channel feed.
    /// Ordinary projection clears preserve a controller-bound gate; only a
    /// known rejection or explicit account/disconnect reset releases it, so a
    /// second click cannot duplicate an in-flight intent.
    pub(super) fn queue_channel_post_reaction(
        &mut self,
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<RemoteChannelPostReactionRequest> {
        let (team, channel) = self.selected_targets()?;
        if self.selected().is_some_and(|(_, channel)| channel.archived)
            || self.post_reaction_is_pending(&team, &channel, &post, reaction)
        {
            return None;
        }
        let current_selection = match &self.messages {
            RemoteChannelMessagesState::Ready(posts) => posts
                .iter()
                .find(|candidate| candidate.target == post && !candidate.deleted)
                .map(|candidate| candidate.reaction_is_selected(reaction)),
            RemoteChannelMessagesState::None
            | RemoteChannelMessagesState::Loading
            | RemoteChannelMessagesState::AccessDenied => None,
        };
        // The page derives `selected` from this same last-applied provider
        // projection. Recheck it at the state boundary so an old UI frame
        // cannot turn a second click into a duplicate set/unset request.
        if current_selection.is_none_or(|current| current == selected) {
            return None;
        }

        self.pending_post_reactions
            .push(PendingRemoteChannelPostReaction {
                team: team.clone(),
                channel: channel.clone(),
                post: post.clone(),
                reaction,
                selected,
                intent_id: None,
            });
        Some(RemoteChannelPostReactionRequest {
            team,
            channel,
            post,
            reaction,
            selected,
        })
    }

    pub(super) fn post_reaction_is_pending(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.pending_post_reactions.iter().any(|pending| {
            pending.team == *team
                && pending.channel == *channel
                && pending.post == *post
                && pending.reaction == reaction
        })
    }

    /// Returns whether an exact root-post reaction click is still dispatchable
    /// from the current provider projection. A controller must call this
    /// immediately before it drains its action vector: a bound tombstone, a
    /// stale/missing/deleted target, a now-matching provider selection, or an
    /// archived channel must never be mistaken for an action to dispatch.
    pub(super) fn has_unbound_channel_post_reaction(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        if !self.selected_channel_is(team, channel)
            || self.selected().is_none_or(|(_, channel)| channel.archived)
        {
            return false;
        }
        let RemoteChannelMessagesState::Ready(posts) = &self.messages else {
            return false;
        };
        let Some(candidate) = posts
            .iter()
            .find(|candidate| candidate.target == *post && !candidate.deleted)
        else {
            return false;
        };
        candidate.reaction_is_selected(reaction) != selected
            && self.pending_post_reactions.iter().any(|pending| {
                pending.intent_id.is_none()
                    && pending.team == *team
                    && pending.channel == *channel
                    && pending.post == *post
                    && pending.reaction == reaction
                    && pending.selected == selected
            })
    }

    /// Binds a queue-allocated intent to the exact reaction request that
    /// produced it. The route check prevents a later same-channel click from
    /// acquiring an earlier command's failure outcome.
    pub(super) fn bind_channel_post_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        let Some(pending) = self.pending_post_reactions.iter_mut().find(|pending| {
            pending.intent_id.is_none()
                && pending.team == team
                && pending.channel == channel
                && pending.post == post
                && pending.reaction == reaction
        }) else {
            return false;
        };
        pending.intent_id = Some(intent_id);
        true
    }

    /// A known provider rejection can safely re-enable only the originating
    /// root post. Accepted and delivery-uncertain requests remain locked until
    /// the matching bounded feed replacement reconciles their visible state.
    pub(super) fn reject_channel_post_reaction(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(index) = self
            .pending_post_reactions
            .iter()
            .position(|pending| pending.intent_id == Some(intent_id))
        else {
            return false;
        };
        self.pending_post_reactions.swap_remove(index);
        true
    }

    /// Whether this root-post reaction intent still holds a gate.
    pub(super) fn owns_post_reaction_intent(&self, intent_id: WorkspaceIntentId) -> bool {
        self.pending_post_reactions
            .iter()
            .any(|pending| pending.intent_id == Some(intent_id))
    }

    /// Stages one bounded-palette reaction change for a visible reply without
    /// changing the rendered provider projection. The full selected
    /// team/channel/root-post/reply/kind identity stays locked until that
    /// exact reaction reports the requested provider-owned selection in a
    /// later matching reply feed. Ordinary selection changes preserve a
    /// controller-bound gate; only a known rejection or explicit
    /// account/disconnect reset releases it.
    pub(super) fn queue_channel_reply_reaction(
        &mut self,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<RemoteChannelReplyReactionRequest> {
        let (team, channel, post) = self.selected_reply_targets()?;
        if self.selected().is_some_and(|(_, channel)| channel.archived)
            || self.reply_reaction_is_pending(&team, &channel, &post, &reply, reaction)
        {
            return None;
        }
        let current_selection = match &self.replies {
            RemoteChannelRepliesState::Ready(replies) => replies
                .iter()
                .find(|candidate| candidate.target == reply && !candidate.deleted)
                .map(|candidate| candidate.reaction_is_selected(reaction)),
            RemoteChannelRepliesState::None
            | RemoteChannelRepliesState::Loading
            | RemoteChannelRepliesState::AccessDenied => None,
        };
        // The page derives `selected` from the same last-applied projection.
        // Recheck at this boundary so a stale UI frame cannot produce a second
        // set/unset request for the same reply.
        if current_selection.is_none_or(|current| current == selected) {
            return None;
        }

        self.pending_reply_reactions
            .push(PendingRemoteChannelReplyReaction {
                team: team.clone(),
                channel: channel.clone(),
                post: post.clone(),
                reply: reply.clone(),
                reaction,
                selected,
                intent_id: None,
            });
        Some(RemoteChannelReplyReactionRequest {
            team,
            channel,
            post,
            reply,
            reaction,
            selected,
        })
    }

    pub(super) fn reply_reaction_is_pending(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reply: &ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.pending_reply_reactions.iter().any(|pending| {
            pending.team == *team
                && pending.channel == *channel
                && pending.post == *post
                && pending.reply == *reply
                && pending.reaction == reaction
        })
    }

    /// Returns whether an exact reply reaction click remains dispatchable from
    /// the current selected-root projection. A route-complete bound tombstone,
    /// stale/missing/deleted target, now-matching provider selection, or
    /// archived channel intentionally returns false so a stale coordinator
    /// vector cannot dispatch it again.
    pub(super) fn has_unbound_channel_reply_reaction(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reply: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        if !self.selected_reply_is(team, channel, post)
            || self.selected().is_none_or(|(_, channel)| channel.archived)
        {
            return false;
        }
        let RemoteChannelRepliesState::Ready(replies) = &self.replies else {
            return false;
        };
        let Some(candidate) = replies
            .iter()
            .find(|candidate| candidate.target == *reply && !candidate.deleted)
        else {
            return false;
        };
        candidate.reaction_is_selected(reaction) != selected
            && self.pending_reply_reactions.iter().any(|pending| {
                pending.intent_id.is_none()
                    && pending.team == *team
                    && pending.channel == *channel
                    && pending.post == *post
                    && pending.reply == *reply
                    && pending.reaction == reaction
                    && pending.selected == selected
            })
    }

    /// Binds a queue-allocated intent to the exact reply route that produced
    /// it. Every provider path segment is checked so another selected reply
    /// cannot receive this command's outcome.
    pub(super) fn bind_channel_reply_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        let Some(pending) = self.pending_reply_reactions.iter_mut().find(|pending| {
            pending.intent_id.is_none()
                && pending.team == team
                && pending.channel == channel
                && pending.post == post
                && pending.reply == reply
                && pending.reaction == reaction
        }) else {
            return false;
        };
        pending.intent_id = Some(intent_id);
        true
    }

    /// A known provider rejection can safely re-enable only the originating
    /// reply. Accepted and delivery-uncertain requests remain locked until a
    /// matching selected reply projection replaces their visible state.
    pub(super) fn reject_channel_reply_reaction(&mut self, intent_id: WorkspaceIntentId) -> bool {
        let Some(index) = self
            .pending_reply_reactions
            .iter()
            .position(|pending| pending.intent_id == Some(intent_id))
        else {
            return false;
        };
        self.pending_reply_reactions.swap_remove(index);
        true
    }

    /// Whether this reply reaction intent still holds a gate.
    pub(super) fn owns_reply_reaction_intent(&self, intent_id: WorkspaceIntentId) -> bool {
        self.pending_reply_reactions
            .iter()
            .any(|pending| pending.intent_id == Some(intent_id))
    }

    /// Converts the current selected channel's session-only draft into an
    /// immutable provider-neutral command request. The visible remote feed is
    /// deliberately not changed here: only a later normalized projection may
    /// show Graph content.
    pub(super) fn queue_channel_post(
        &mut self,
    ) -> Result<RemoteChannelPostSendRequest, RemoteChannelPostError> {
        let (team, channel) = self
            .selected_targets()
            .ok_or(RemoteChannelPostError::MissingSelection)?;
        if self.selected().is_some_and(|(_, channel)| channel.archived) {
            return Err(RemoteChannelPostError::Archived);
        }
        self.delivery.queue_root_post(team, channel)
    }

    pub(super) fn bind_channel_post_submission(
        &mut self,
        submission: RemoteChannelPostSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery
            .bind_root_post_submission(submission, intent_id)
    }

    pub(super) fn resolve_channel_post_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.delivery.root_post_delivered(intent_id)
    }

    pub(super) fn resolve_channel_post_retry_scheduled(
        &self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery.root_post_retry_scheduled(intent_id)
    }

    pub(super) fn resolve_channel_post_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.delivery.root_post_failed(intent_id)
    }

    pub(super) fn resolve_channel_post_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery.root_post_delivery_uncertain(intent_id)
    }

    pub(super) fn make_uncertain_channel_post_editable(&mut self) -> bool {
        let Some(target) = self.selected_channel_post_delivery_target() else {
            return false;
        };
        self.delivery.make_unknown_editable(&target)
    }

    fn channel_post_is_pending_elsewhere(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) -> bool {
        let target = RemoteChannelDeliveryTarget::root_post(team.clone(), channel.clone());
        self.delivery.has_active_elsewhere(&target)
    }

    fn selected_channel_post_composer_mut(
        &mut self,
    ) -> Option<&mut super::remote_delivery::RemoteChannelDeliveryComposer> {
        let target = self.selected_channel_post_delivery_target()?;
        self.delivery.composer_for_target_mut(&target)
    }

    fn stash_idle_remote_delivery(&mut self) {
        self.delivery.stash_idle();
    }

    fn selected_channel_post_delivery_target(&self) -> Option<RemoteChannelDeliveryTarget> {
        let (team, channel) = self.selected_targets()?;
        Some(RemoteChannelDeliveryTarget::root_post(team, channel))
    }

    /// Converts the selected root post's text-only draft into a provider-
    /// neutral reply command. No reply-to-reply target can enter this path.
    pub(super) fn queue_channel_reply(
        &mut self,
    ) -> Result<RemoteChannelReplySendRequest, RemoteChannelDeliveryError> {
        let (team, channel, post) = self
            .selected_reply_targets()
            .ok_or(RemoteChannelDeliveryError::MissingSelection)?;
        if self.selected().is_some_and(|(_, channel)| channel.archived) {
            return Err(RemoteChannelDeliveryError::Archived);
        }
        self.delivery.queue_reply(team, channel, post)
    }

    pub(super) fn bind_channel_reply_submission(
        &mut self,
        submission: RemoteChannelReplySubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery.bind_reply_submission(submission, intent_id)
    }

    pub(super) fn resolve_channel_reply_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.delivery.reply_delivered(intent_id)
    }

    pub(super) fn resolve_channel_reply_retry_scheduled(
        &self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery.reply_retry_scheduled(intent_id)
    }

    pub(super) fn resolve_channel_reply_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.delivery.reply_failed(intent_id)
    }

    pub(super) fn resolve_channel_reply_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.delivery.reply_delivery_uncertain(intent_id)
    }

    pub(super) fn make_uncertain_channel_reply_editable(&mut self) -> bool {
        let Some(target) = self.selected_reply_delivery_target() else {
            return false;
        };
        self.delivery.make_unknown_editable(&target)
    }

    fn selected_reply_delivery_target(&self) -> Option<RemoteChannelDeliveryTarget> {
        let (team, channel, post) = self.selected_reply_targets()?;
        Some(RemoteChannelDeliveryTarget::reply(team, channel, post))
    }

    fn channel_reply_is_pending_elsewhere(&self) -> bool {
        self.selected_reply_delivery_target()
            .is_some_and(|target| self.delivery.has_active_elsewhere(&target))
    }

    fn selected_channel_reply_composer_mut(
        &mut self,
    ) -> Option<&mut super::remote_delivery::RemoteChannelDeliveryComposer> {
        let target = self.selected_reply_delivery_target()?;
        self.delivery.composer_for_target_mut(&target)
    }

    /// Changes the selected remote channel back to its root-post surface.
    /// Existing post/reply and file snapshots remain session-only and intact.
    pub(super) fn show_posts(&mut self) {
        self.surface = RemoteChannelSurface::Posts;
    }

    /// Shows channel-folder metadata. The first explicit visit requests one
    /// bounded read; revisiting an already loaded/denied/loading view only
    /// changes presentation and does not create a duplicate provider read.
    pub(super) fn show_files(&mut self) -> bool {
        if self.selection.is_none() {
            return false;
        }
        self.surface = RemoteChannelSurface::Files;
        if matches!(self.files, RemoteChannelFilesState::None) {
            self.files = RemoteChannelFilesState::Loading;
            return true;
        }
        false
    }

    /// Reissues the exact same selected-channel files read. This permits a
    /// retry after an adapter error that did not produce a projection, while
    /// the app-owned controller deduplicates an already active/queued target.
    pub(super) fn refresh_files(&mut self) -> bool {
        if self.selection.is_none() {
            return false;
        }
        self.surface = RemoteChannelSurface::Files;
        self.files = RemoteChannelFilesState::Loading;
        true
    }

    pub(super) const fn is_showing_files(&self) -> bool {
        matches!(self.surface, RemoteChannelSurface::Files)
    }

    /// Selects a post only after confirming it belongs to the visible
    /// selected-channel root feed. The state transition is read-only and
    /// never opens a local thread, composer, or reaction path.
    pub(super) fn select_post(&mut self, post: ProviderResourceRef) -> bool {
        let post_is_visible = match &self.messages {
            RemoteChannelMessagesState::Ready(posts) => {
                posts.iter().any(|candidate| candidate.target == post)
            }
            RemoteChannelMessagesState::None
            | RemoteChannelMessagesState::Loading
            | RemoteChannelMessagesState::AccessDenied => false,
        };
        if !post_is_visible || self.selected_post_is(&post) {
            return false;
        }
        // A reply reaction can remain queued or in flight while the user
        // views another root post. Keep bound routes as tombstones until
        // their exact reply projection confirms the requested selection.
        self.clear_replies_preserving_bound_reactions();
        self.reply_selection = Some(RemoteChannelPostSelection { post });
        self.replies = RemoteChannelRepliesState::Loading;
        true
    }

    pub(super) fn selected_reply_targets(
        &self,
    ) -> Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )> {
        let selection = self.selection.as_ref()?;
        let post = self.reply_selection.as_ref()?;
        Some((
            selection.team.clone(),
            selection.channel.clone(),
            post.post.clone(),
        ))
    }

    /// Starts a refresh of the selected channel's root-post feed that keeps
    /// the loaded rows visible (no Loading flash). Only a `Ready` feed
    /// refreshes. Bound reaction gates and tombstones are untouched: the
    /// next exact feed still has to confirm them. A failed read leaves the
    /// rows as they were.
    pub(super) fn begin_messages_refresh(
        &mut self,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        if !matches!(self.messages, RemoteChannelMessagesState::Ready(_)) {
            return None;
        }
        self.selected_targets()
    }

    /// The same keep-rows refresh for the selected root post's replies.
    pub(super) fn begin_replies_refresh(
        &mut self,
    ) -> Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )> {
        if !matches!(self.replies, RemoteChannelRepliesState::Ready(_)) {
            return None;
        }
        self.selected_reply_targets()
    }

    /// Requests a fresh read for the same selected root post. It intentionally
    /// retains the opaque route and does not emit a cancellation signal for
    /// an active same-target reply poll; the app-owned controller deduplicates
    /// a repeated same-target request while a provider read is still queued.
    pub(super) fn refresh_replies(&mut self) -> bool {
        if self.reply_selection.is_none() {
            return false;
        }
        // A refresh retains only controller-owned mutations for this exact
        // selected root. A click that never received an intent cannot safely
        // survive a new frame or be attributed to a later command outcome.
        self.retain_bound_reply_reactions();
        self.replies = RemoteChannelRepliesState::Loading;
        true
    }

    fn selected_post_is(&self, post: &ProviderResourceRef) -> bool {
        self.reply_selection
            .as_ref()
            .is_some_and(|selection| selection.post == *post)
    }

    fn selected_reply_is(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
    ) -> bool {
        self.selected_channel_is(team, channel) && self.selected_post_is(post)
    }

    /// Drops selected-root-post state and asks the owner of the provider
    /// worker to cancel it if a reply read was active or queued. This is used
    /// at an explicit account/disconnect boundary, where every gate is safe
    /// to abandon along with the worker-owned commands.
    fn clear_replies(&mut self) {
        // A reply read can be cancelled independently of the non-idempotent
        // write state. Stash only an ordinary draft; pending/unknown delivery
        // remains attached to its exact opaque target.
        self.delivery.stash_idle();
        if self.reply_selection.take().is_some()
            || !matches!(self.replies, RemoteChannelRepliesState::None)
        {
            self.replies_clear_requested = true;
        }
        self.replies = RemoteChannelRepliesState::None;
        self.clear_pending_reply_reactions();
    }

    /// Drops visible selected-root-post state while preserving only
    /// controller-bound reaction gates. Navigation, a transient feed omission,
    /// and transport loss cannot prove that a non-idempotent mutation did not
    /// reach Microsoft Graph, so its exact opaque route remains locked until
    /// a later matching reply projection reconciles it.
    fn clear_replies_preserving_bound_reactions(&mut self) {
        self.delivery.stash_idle();
        if self.reply_selection.take().is_some()
            || !matches!(self.replies, RemoteChannelRepliesState::None)
        {
            self.replies_clear_requested = true;
        }
        self.replies = RemoteChannelRepliesState::None;
        self.retain_bound_reply_reactions();
    }

    /// Clears only the visible reply projection for a refresh of the same
    /// selected channel. The selected root remains the same opaque route, so
    /// a controller-bound reply reaction must remain gated until its matching
    /// reply projection confirms the requested typed selection.
    fn clear_replies_for_same_channel_refresh(&mut self) {
        self.delivery.stash_idle();
        self.retain_bound_reply_reactions();
        self.replies = if self.reply_selection.is_some() {
            RemoteChannelRepliesState::Loading
        } else {
            RemoteChannelRepliesState::None
        };
    }

    /// Drops only the selected channel's folder metadata and asks the owner
    /// of the provider worker to cancel it if the bounded read was queued or
    /// active. It intentionally leaves root posts and replies untouched.
    fn clear_files(&mut self) {
        if !matches!(self.files, RemoteChannelFilesState::None) {
            self.files_clear_requested = true;
        }
        self.files = RemoteChannelFilesState::None;
        self.pending_channel_file_open_links.clear();
    }

    /// Reconciles reaction gates against an incoming same-route root-post
    /// projection. A matching provider-owned selection releases either gate.
    /// If the target disappeared or was deleted in that authoritative feed,
    /// an unbound UI click is abandoned while a controller-bound tombstone
    /// remains locked: its non-idempotent write may still resolve after the
    /// target reappears.
    /// A response for another channel or a stale/cross-kind copy cannot make
    /// a duplicate click available.
    fn resolve_pending_post_reactions(&mut self, directory: &RemoteChannelMessagesDirectory) {
        let RemoteChannelMessagesAvailability::Available = directory.availability else {
            return;
        };
        self.pending_post_reactions.retain(|pending| {
            if pending.team != directory.team || pending.channel != directory.channel {
                return true;
            }
            let Some(post) = directory
                .posts
                .iter()
                .find(|post| post.target == pending.post && !post.deleted)
            else {
                return pending.intent_id.is_some();
            };
            post.reaction_is_selected(pending.reaction) != pending.selected
        });
    }

    fn clear_pending_post_reactions(&mut self) {
        self.pending_post_reactions.clear();
    }

    /// Abandons feature-only clicks for an archived channel while retaining
    /// every controller-bound route as a delivery tombstone. This is narrower
    /// than a reset: reactions from other previously visited channels remain
    /// available for their own eventual reconciliation.
    fn discard_unbound_reactions_for_channel(
        &mut self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) {
        self.pending_post_reactions.retain(|pending| {
            pending.intent_id.is_some() || pending.team != *team || pending.channel != *channel
        });
        self.pending_reply_reactions.retain(|pending| {
            pending.intent_id.is_some() || pending.team != *team || pending.channel != *channel
        });
    }

    /// Keeps only mutations that the app controller has accepted and bound to
    /// an opaque intent. Refreshes, ordinary navigation, temporary projection
    /// loss, and transport loss retain these route-specific tombstones; an
    /// explicit account/disconnect boundary clears every pending gate.
    fn retain_bound_post_reactions(&mut self) {
        self.pending_post_reactions
            .retain(|pending| pending.intent_id.is_some());
    }

    /// Reconciles reply-reaction gates against an incoming selected-root-post
    /// projection. A matching provider-owned selection releases either gate.
    /// If the reply disappeared or was deleted from that authoritative route,
    /// only an unbound click is abandoned; a controller-bound tombstone stays
    /// locked until its eventual projection/rejection outcome. Another root
    /// post, channel, denied response, or stale/cross-kind copy cannot make a
    /// duplicate click available.
    fn resolve_pending_reply_reactions(&mut self, directory: &RemoteChannelRepliesDirectory) {
        let RemoteChannelRepliesAvailability::Available = directory.availability else {
            return;
        };
        self.pending_reply_reactions.retain(|pending| {
            if pending.team != directory.team
                || pending.channel != directory.channel
                || pending.post != directory.post
            {
                return true;
            }
            let Some(reply) = directory
                .replies
                .iter()
                .find(|reply| reply.target == pending.reply && !reply.deleted)
            else {
                return pending.intent_id.is_some();
            };
            reply.reaction_is_selected(pending.reaction) != pending.selected
        });
    }

    fn clear_pending_reply_reactions(&mut self) {
        self.pending_reply_reactions.clear();
    }

    /// Same as [`Self::retain_bound_post_reactions`] for selected-root replies.
    fn retain_bound_reply_reactions(&mut self) {
        self.pending_reply_reactions
            .retain(|pending| pending.intent_id.is_some());
    }

    #[cfg(test)]
    pub(super) const fn is_loading_messages(&self) -> bool {
        matches!(self.messages, RemoteChannelMessagesState::Loading)
    }

    #[cfg(test)]
    pub(super) const fn has_message_access_denied(&self) -> bool {
        matches!(self.messages, RemoteChannelMessagesState::AccessDenied)
    }

    #[cfg(test)]
    pub(super) fn loaded_post_count(&self) -> usize {
        match &self.messages {
            RemoteChannelMessagesState::Ready(posts) => posts.len(),
            RemoteChannelMessagesState::None
            | RemoteChannelMessagesState::Loading
            | RemoteChannelMessagesState::AccessDenied => 0,
        }
    }

    #[cfg(test)]
    pub(super) const fn is_loading_replies(&self) -> bool {
        matches!(self.replies, RemoteChannelRepliesState::Loading)
    }

    #[cfg(test)]
    pub(super) const fn has_reply_access_denied(&self) -> bool {
        matches!(self.replies, RemoteChannelRepliesState::AccessDenied)
    }

    #[cfg(test)]
    pub(super) fn loaded_reply_count(&self) -> usize {
        match &self.replies {
            RemoteChannelRepliesState::Ready(replies) => replies.len(),
            RemoteChannelRepliesState::None
            | RemoteChannelRepliesState::Loading
            | RemoteChannelRepliesState::AccessDenied => 0,
        }
    }

    #[cfg(test)]
    pub(super) const fn is_loading_files(&self) -> bool {
        matches!(self.files, RemoteChannelFilesState::Loading)
    }

    #[cfg(test)]
    pub(super) const fn has_files_access_denied(&self) -> bool {
        matches!(self.files, RemoteChannelFilesState::AccessDenied)
    }

    #[cfg(test)]
    pub(super) fn loaded_file_count(&self) -> usize {
        match &self.files {
            RemoteChannelFilesState::Ready(files) => files.len(),
            RemoteChannelFilesState::None
            | RemoteChannelFilesState::Loading
            | RemoteChannelFilesState::AccessDenied => 0,
        }
    }

    #[cfg(test)]
    pub(super) fn set_channel_post_draft_for_test(&mut self, draft: &str) -> bool {
        let Some(composer) = self.selected_channel_post_composer_mut() else {
            return false;
        };
        if composer.pending.is_some() || composer.delivery_unknown.is_some() {
            return false;
        }
        composer.draft = draft.into();
        true
    }

    #[cfg(test)]
    pub(super) fn channel_post_draft_for_test(&self) -> Option<&str> {
        let target = self.selected_channel_post_delivery_target()?;
        self.delivery
            .composer_for_target(&target)
            .map(|composer| composer.draft.as_str())
    }

    #[cfg(test)]
    pub(super) fn has_pending_channel_post_for_test(&self) -> bool {
        self.delivery.has_active_root_post_for_test()
    }

    #[cfg(test)]
    pub(super) fn has_unknown_channel_post_for_test(&self) -> bool {
        self.delivery.has_unknown_root_post_for_test()
    }

    #[cfg(test)]
    pub(super) fn set_channel_reply_draft_for_test(&mut self, draft: &str) -> bool {
        let Some(composer) = self.selected_channel_reply_composer_mut() else {
            return false;
        };
        if composer.pending.is_some() || composer.delivery_unknown.is_some() {
            return false;
        }
        composer.draft = draft.into();
        true
    }

    #[cfg(test)]
    pub(super) fn channel_reply_draft_for_test(&self) -> Option<&str> {
        let target = self.selected_reply_delivery_target()?;
        self.delivery
            .composer_for_target(&target)
            .map(|composer| composer.draft.as_str())
    }

    #[cfg(test)]
    pub(super) fn has_pending_channel_reply_for_test(&self) -> bool {
        self.delivery.has_active_reply_for_test()
    }

    #[cfg(test)]
    pub(super) fn has_unknown_channel_reply_for_test(&self) -> bool {
        self.delivery.has_unknown_reply_for_test()
    }
}
