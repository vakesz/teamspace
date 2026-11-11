//! Teams feature facade and durable local domain state.
//!
//! Rendering is isolated in sibling modules; this file owns the narrow
//! workspace-facing interface, stable identifiers, and persistence projection.

mod page;
mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;
mod remote;
mod remote_delivery;
mod sidebar;
mod thread;

pub(crate) use self::page::draw_page;
#[cfg(test)]
use self::page::{draw_channel_composer, draw_teams_posts_page};
pub(crate) use self::persistence::{LocalTeamsStore, StoredTeams};
use self::persistence::{
    MAX_POST_BYTES, StoredChannelState, StoredLocalPost, TEAMS_SCHEMA_VERSION,
    local_post_id_from_key, local_post_key, snapshot_fits_local_store,
};
pub(crate) use self::remote::{
    RemoteChannelDirectory, RemoteChannelFile, RemoteChannelFilesDirectory,
    RemoteChannelMessagesDirectory, RemoteChannelPost, RemoteChannelRepliesDirectory,
    RemoteChannelReply, RemoteTeamDirectory, RemoteTeamsDirectory,
};
pub(crate) use self::remote_delivery::{
    RemoteChannelPostSubmissionId, RemoteChannelReplySubmissionId,
};
#[cfg(test)]
use self::sidebar::draw_community_picker;
pub(crate) use self::sidebar::draw_sidebar;
use self::thread::source_reply_reaction_key;
#[cfg(test)]
use self::thread::{draw_inline_post_thread, thread_reply_link_label};

use crate::data::team_catalog::{
    SourceReply, TeamDefinition, channel_for_key, channel_key, team_catalog, team_for_channel,
};
use crate::features::search::workspace::{WorkspaceSearchResult, WorkspaceSearchTarget};
use crate::features::threads::{
    DetachedThread, LocalThreadReply, ThreadError, ThreadId, ThreadStore,
    ThreadTarget as DiscussionTarget,
};
use crate::integrations::{ProviderBrowserUrl, ProviderResourceRef, WorkspaceIntentId};
use crate::shared::reaction::ReactionKind;
use crate::ui::link_preview::LinkPreview;
use crate::ui::reactions::{ReactionSeed, ReactionSet};
use std::collections::{BTreeMap, BTreeSet};

const NO_REACTIONS: &[ReactionSeed] = &[];
/// Seeded community content is reference data, not a profile membership.
///
/// Keep it outside the locally persisted `joined_communities` list so profile
/// actions never imply a provider-side leave operation.
pub(super) const REFERENCE_COMMUNITY_NAME: &str = "Whole Studio Community";

/// Returns whether a name denotes the source-owned reference community.
///
/// Community names are display text, so use the same trimmed,
/// case-insensitive identity a user would perceive rather than making the
/// source/local boundary depend on one exact spelling in a cache file.
pub(super) fn is_reference_community(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(REFERENCE_COMMUNITY_NAME)
}

/// Whether a canonical discussion is rooted at a provider-addressable Teams
/// catalog post. Local channel posts have no provider message ID yet, and
/// Activity-only `activity:*` records must remain local even when their
/// channel name belongs to a team.
pub(crate) fn is_transportable_thread(thread: &ThreadId) -> bool {
    is_transportable_thread_target(thread, &DiscussionTarget::Post)
}

/// Whether one exact canonical mutation has a provider-addressable Teams
/// target. A source reply is eligible only when it belongs to the same
/// catalogued post; local replies never have a provider-issued identifier.
pub(crate) fn is_transportable_thread_target(thread: &ThreadId, target: &DiscussionTarget) -> bool {
    let Some(post) = channel_for_key(thread.channel()).and_then(|channel| {
        channel
            .posts
            .iter()
            .find(|post| post.key == thread.post_key())
    }) else {
        return false;
    };
    match target {
        DiscussionTarget::Post => true,
        DiscussionTarget::SourceReply { reply_key } => post
            .source_replies
            .iter()
            .any(|reply| reply.key == reply_key),
        DiscussionTarget::LocalReply { .. } => false,
    }
}

/// Stateful controls owned only by the Teams surface.
/// The most recently deleted local channel post with everything that went
/// with it, held in memory for the Undo toast. The delete is persisted at once;
/// undo re-inserts and persists again.
struct DeletedLocalPost {
    channel: String,
    index: usize,
    post: LocalPost,
    key: String,
    legacy_replies: Option<Vec<String>>,
    legacy_reactions: Option<Vec<String>>,
    thread: Option<DetachedThread>,
}

pub(crate) struct TeamsState {
    undo_post: Option<DeletedLocalPost>,
    selected_channel: String,
    channel_state: BTreeMap<String, ChannelState>,
    active_thread: Option<ThreadTarget>,
    thread_draft: String,
    editing_reply: Option<LocalReplyEditor>,
    team_tab: TeamTab,
    team_search_open: bool,
    team_search_query: String,
    team_filter_open: bool,
    team_filter_query: String,
    teams_and_channels_open: bool,
    communities_open: bool,
    expanded_teams: BTreeSet<String>,
    community_picker_open: bool,
    joined_communities: Vec<String>,
    /// Provider-backed Teams hierarchy, kept strictly session-only so it
    /// cannot be folded into the local preview catalog or persistence file.
    remote: remote::RemoteTeamsState,
    link_preview: Option<LinkPreview>,
    next_local_post_id: u64,
    persistence_dirty: bool,
}

impl Default for TeamsState {
    fn default() -> Self {
        Self {
            undo_post: None,
            selected_channel: "Bike Commuters · General".into(),
            channel_state: BTreeMap::new(),
            active_thread: None,
            thread_draft: String::new(),
            editing_reply: None,
            team_tab: TeamTab::Posts,
            team_search_open: false,
            team_search_query: String::new(),
            team_filter_open: false,
            team_filter_query: String::new(),
            teams_and_channels_open: true,
            communities_open: true,
            expanded_teams: team_catalog().iter().map(|team| team.name.into()).collect(),
            community_picker_open: false,
            joined_communities: Vec::new(),
            remote: remote::RemoteTeamsState::default(),
            link_preview: None,
            next_local_post_id: 1,
            persistence_dirty: false,
        }
    }
}

impl TeamsState {
    pub(crate) fn selected_channel_name(&self) -> &str {
        &self.selected_channel
    }

    /// Applies a fully normalized Microsoft 365 directory for this session.
    /// It never mutates the bundled catalog, the selected local channel, or
    /// the durable Teams snapshot.
    pub(crate) fn replace_remote_directory(&mut self, directory: RemoteTeamsDirectory) {
        self.remote.replace(directory);
    }

    /// Drops the remote account projection on disconnect/profile change.
    /// Local Teams state deliberately remains intact.
    pub(crate) fn clear_remote_directory(&mut self) {
        self.remote.clear();
    }

    /// Drops a failed remote projection without discarding an already accepted
    /// root-post or reply draft. The transport still owns its eventual
    /// delivery result, which may be ambiguous; explicit disconnect/profile
    /// reset must use `clear_remote_directory` instead.
    pub(crate) fn clear_remote_directory_preserving_delivery_recovery(&mut self) {
        self.remote.clear_for_transport_loss();
    }

    /// Replaces the selected remote channel's session-only root-post feed.
    /// A mismatched selection is ignored so an older worker cannot overwrite
    /// the channel the user is currently viewing.
    pub(crate) fn replace_remote_channel_messages(
        &mut self,
        directory: RemoteChannelMessagesDirectory,
    ) -> bool {
        self.remote.replace_messages(directory)
    }

    /// Replaces the selected root post's session-only reply view. A response
    /// must match every opaque team, channel, and root-post target or it is
    /// ignored as stale.
    pub(crate) fn replace_remote_channel_replies(
        &mut self,
        directory: RemoteChannelRepliesDirectory,
    ) -> bool {
        self.remote.replace_replies(directory)
    }

    /// Replaces the selected channel's session-only file metadata. A response
    /// for a different opaque team/channel target is ignored as stale.
    pub(crate) fn replace_remote_channel_files(
        &mut self,
        directory: RemoteChannelFilesDirectory,
    ) -> bool {
        self.remote.replace_files(directory)
    }

    /// Returns whether this exact selected remote channel still owns a
    /// selected-root reply view. The workspace coordinator uses this to keep
    /// a same-channel reply refresh alive while cancelling an initial or
    /// changed-channel read.
    pub(crate) fn has_selected_remote_channel_reply_for(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) -> bool {
        self.remote.has_selected_reply_for(team, channel)
    }

    /// Starts an explicit keep-rows refresh of the selected channel's posts.
    pub(crate) fn begin_remote_channel_messages_refresh(
        &mut self,
    ) -> Option<(ProviderResourceRef, ProviderResourceRef)> {
        self.remote.begin_messages_refresh()
    }

    /// Starts an explicit keep-rows refresh of the selected post's replies.
    pub(crate) fn begin_remote_channel_replies_refresh(
        &mut self,
    ) -> Option<(
        ProviderResourceRef,
        ProviderResourceRef,
        ProviderResourceRef,
    )> {
        self.remote.begin_replies_refresh()
    }

    /// Clears only transient remote root posts. The local catalog and stored
    /// Teams state remain unchanged, and an active remote selection returns to
    /// its loading state until a matching provider projection arrives. A
    /// controller-bound reaction remains locked through this same-selection
    /// refresh; only an unbound UI click is discarded.
    pub(crate) fn clear_remote_channel_messages(&mut self) {
        self.remote.clear_messages();
    }

    /// Correlates a feature-local remote root-post submission with the opaque
    /// controller intent allocated when its command enters the transport FIFO.
    pub(crate) fn bind_remote_channel_post_submission(
        &mut self,
        submission: RemoteChannelPostSubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote
            .bind_channel_post_submission(submission, intent_id)
    }

    /// Binds the transport FIFO intent allocated for one exact remote
    /// root-post reaction. Unlike a composer submission, a reaction has no
    /// durable draft or feature-local submission ID, so its opaque route is
    /// verified again at this handoff.
    pub(crate) fn bind_remote_channel_post_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.remote
            .bind_channel_post_reaction(intent_id, team, channel, post, reaction)
    }

    /// Returns whether one exact root-post reaction is still dispatchable from
    /// the current feature projection. The workspace coordinator uses this
    /// immediately before dispatching its queued action so a dropped/stale
    /// gate, matching provider state, archived channel, or bound tombstone
    /// cannot produce a duplicate mutation.
    pub(crate) fn has_unbound_remote_channel_post_reaction(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        self.remote
            .has_unbound_channel_post_reaction(team, channel, post, reaction, selected)
    }

    /// Releases only a reaction whose exact transport intent received a known
    /// rejection. Accepted or delivery-uncertain reactions remain session-only
    /// locked until a matching remote root-post projection is applied.
    pub(crate) fn remote_channel_post_reaction_failed(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote.reject_channel_post_reaction(intent_id)
    }

    /// Whether this exact root-post reaction intent is owned by the feature. A
    /// delivered, throttled-retry or uncertain outcome keeps its gate until a
    /// matching root-post projection supplies the provider state.
    pub(crate) fn remote_channel_post_reaction_owned(&self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.owns_post_reaction_intent(intent_id)
    }

    /// Binds the transport FIFO intent allocated for one exact remote reply
    /// reaction. The full selected team/channel/root-post/reply route is
    /// verified so a result cannot be attributed to a neighbouring reply.
    pub(crate) fn bind_remote_channel_reply_reaction(
        &mut self,
        intent_id: WorkspaceIntentId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
    ) -> bool {
        self.remote
            .bind_channel_reply_reaction(intent_id, team, channel, post, reply, reaction)
    }

    /// Returns whether one exact direct-reply reaction is still dispatchable
    /// from the current feature projection. Bound tombstones, stale provider
    /// state, and archived channels intentionally do not match: their command
    /// must not be dispatched again.
    pub(crate) fn has_unbound_remote_channel_reply_reaction(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
        post: &ProviderResourceRef,
        reply: &ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> bool {
        self.remote
            .has_unbound_channel_reply_reaction(team, channel, post, reply, reaction, selected)
    }

    /// Releases only a reply reaction whose exact transport intent received a
    /// known rejection. Delivered and delivery-uncertain reactions remain
    /// locked until a matching selected reply projection is applied.
    pub(crate) fn remote_channel_reply_reaction_failed(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote.reject_channel_reply_reaction(intent_id)
    }

    /// Whether this exact reply reaction intent is owned by the feature. A
    /// delivered, throttled-retry or uncertain outcome keeps its gate until a
    /// matching reply feed supplies the provider state.
    pub(crate) fn remote_channel_reply_reaction_owned(&self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.owns_reply_reaction_intent(intent_id)
    }

    /// Clears the locked remote composer after the provider confirms a 201.
    /// It intentionally does not append a local post; the next normalized
    /// remote projection owns the visible feed.
    pub(crate) fn remote_channel_post_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.resolve_channel_post_delivered(intent_id)
    }

    /// Keeps a throttled draft locked while the transport owns its known-safe
    /// retry timing. A user cannot submit a duplicate from the feature.
    pub(crate) fn remote_channel_post_retry_scheduled(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote.resolve_channel_post_retry_scheduled(intent_id)
    }

    /// Restores the exact text only after a definite provider rejection.
    pub(crate) fn remote_channel_post_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.resolve_channel_post_failed(intent_id)
    }

    /// Marks a network/server outcome as explicitly unknown. This is not
    /// reconciled from the bounded feed and cannot automatically resend.
    pub(crate) fn remote_channel_post_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote
            .resolve_channel_post_delivery_uncertain(intent_id)
    }

    /// Correlates one selected-root-post reply command with the intent owned
    /// by the serial workspace transport.
    pub(crate) fn bind_remote_channel_reply_submission(
        &mut self,
        submission: RemoteChannelReplySubmissionId,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote
            .bind_channel_reply_submission(submission, intent_id)
    }

    /// Applies a confirmed `201 Created` outcome for a remote root-post
    /// reply. It never creates a local reply or persistence mutation.
    pub(crate) fn remote_channel_reply_delivered(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.resolve_channel_reply_delivered(intent_id)
    }

    /// Keeps a known-safe throttled reply locked while the transport owns its
    /// retry schedule.
    pub(crate) fn remote_channel_reply_retry_scheduled(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote.resolve_channel_reply_retry_scheduled(intent_id)
    }

    /// Restores a reply's exact retained text after a definite provider
    /// rejection. The focused delivery module provides this behavior.
    pub(crate) fn remote_channel_reply_failed(&mut self, intent_id: WorkspaceIntentId) -> bool {
        self.remote.resolve_channel_reply_failed(intent_id)
    }

    /// Marks a reply delivery as unknown after an ambiguous network/server
    /// result. It must remain locked until local manual review.
    pub(crate) fn remote_channel_reply_delivery_uncertain(
        &mut self,
        intent_id: WorkspaceIntentId,
    ) -> bool {
        self.remote
            .resolve_channel_reply_delivery_uncertain(intent_id)
    }

    /// Drains a feature-owned request to cancel a selected-post reply read
    /// which lost its target because navigation or a root-feed transition
    /// cleared the transient reply selection.
    pub(crate) fn take_remote_channel_replies_clear_requested(&mut self) -> bool {
        self.remote.take_replies_clear_requested()
    }

    /// Drains a feature-owned request to cancel a bounded channel-files read
    /// which lost its selected team/channel target. This never concerns post
    /// or reply work.
    pub(crate) fn take_remote_channel_files_clear_requested(&mut self) -> bool {
        self.remote.take_files_clear_requested()
    }

    /// Drains direct user requests to open one current Microsoft 365 channel
    /// item in the system browser. The opaque provider destination remains
    /// session-only and must be revalidated by the app shell immediately
    /// before launch.
    pub(crate) fn take_remote_channel_file_open_links(&mut self) -> Vec<ProviderBrowserUrl> {
        self.remote.take_channel_file_open_links()
    }

    #[cfg(test)]
    pub(crate) const fn has_remote_directory(&self) -> bool {
        !self.remote.is_empty()
    }

    /// Selects an opaque remote channel through the same feature transition
    /// as a sidebar click. This stays test-only because production selection
    /// is intentionally owned by the rendered directory.
    pub(crate) fn remote_channel_is_selected(
        &self,
        team: &ProviderResourceRef,
        channel: &ProviderResourceRef,
    ) -> bool {
        self.remote.selected_channel_is(team, channel)
    }

    /// Selects the remote channel a Microsoft 365 search hit points at, by
    /// its opaque team and channel targets, through the same transition as a
    /// sidebar click. `false` when the directory does not contain it.
    pub(crate) fn open_remote_channel(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        let changed = self.remote.select(team.clone(), channel.clone());
        if changed {
            self.on_remote_channel_selected();
        }
        changed || self.remote.selected_channel_is(&team, &channel)
    }

    #[cfg(test)]
    pub(crate) fn select_remote_channel_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        let changed = self.remote.select_for_test(team, channel);
        if changed {
            self.on_remote_channel_selected();
        }
        changed
    }

    /// Test-only counterpart of the explicit read-only `View replies`
    /// control. It validates all route segments and never creates a local
    /// thread, composer, or reaction state.
    #[cfg(test)]
    pub(crate) fn select_remote_channel_post_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) -> bool {
        if !self.remote.selected_channel_is(&team, &channel) {
            return false;
        }
        self.remote.select_post(post)
    }

    /// Reaches the same typed root-post reaction action as the rendered
    /// remote feed. Coordinator tests use this instead of forging an action,
    /// so their handoff assertions retain the feature-owned unbound gate.
    #[cfg(test)]
    pub(crate) fn remote_channel_post_reaction_action_for_test(
        &mut self,
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<TeamsAction> {
        self.remote_channel_post_reaction_action(post, reaction, selected)
    }

    /// Reaches the same typed direct-reply reaction action as the rendered
    /// remote reply feed. It intentionally requires the selected-root state
    /// established by the ordinary test selection helper above.
    #[cfg(test)]
    pub(crate) fn remote_channel_reply_reaction_action_for_test(
        &mut self,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<TeamsAction> {
        self.remote_channel_reply_reaction_action(reply, reaction, selected)
    }

    /// Stages exact text in the currently selected remote root-post reply
    /// composer for crate-level coordinator tests. Production code can reach
    /// this state only through the rendered text-only composer.
    #[cfg(test)]
    pub(crate) fn set_remote_channel_reply_draft_for_test(&mut self, draft: &str) -> bool {
        self.remote.set_channel_reply_draft_for_test(draft)
    }

    /// Creates a real pending reply submission for crate-level transport
    /// tests after they have selected a remote root post and staged a draft.
    /// Returning the feature action prevents a forged ID from accidentally
    /// making a binding assertion pass.
    #[cfg(test)]
    pub(crate) fn remote_channel_reply_submission_for_test(&mut self) -> Option<TeamsAction> {
        self.remote_channel_reply_submission_action().ok()
    }

    /// Test-only counterpart of selecting the remote Files surface. It binds
    /// the request to the current opaque channel and starts a first bounded
    /// read only when no session-only metadata snapshot exists yet.
    #[cfg(test)]
    pub(crate) fn select_remote_channel_files_for_test(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) -> bool {
        self.remote.selected_channel_is(&team, &channel) && self.remote.show_files()
    }

    /// Returns the selected remote feed's visible post count for lifecycle
    /// tests without exposing the session-only feed to production callers.
    #[cfg(test)]
    pub(crate) fn remote_channel_message_count(&self) -> usize {
        self.remote.loaded_post_count()
    }

    /// Returns only a count so tests can prove reply delivery stayed
    /// session-only without exposing provider reply content to production.
    #[cfg(test)]
    pub(crate) fn remote_channel_reply_count(&self) -> usize {
        self.remote.loaded_reply_count()
    }

    /// Returns only a count so tests can prove channel-file metadata remains
    /// session-only without exposing provider rows to production callers.
    #[cfg(test)]
    pub(crate) fn remote_channel_file_count(&self) -> usize {
        self.remote.loaded_file_count()
    }

    #[cfg(test)]
    pub(crate) const fn is_loading_remote_channel_replies(&self) -> bool {
        self.remote.is_loading_replies()
    }

    #[cfg(test)]
    pub(crate) const fn has_remote_channel_reply_access_denied(&self) -> bool {
        self.remote.has_reply_access_denied()
    }

    #[cfg(test)]
    pub(crate) const fn is_loading_remote_channel_files(&self) -> bool {
        self.remote.is_loading_files()
    }

    #[cfg(test)]
    pub(crate) const fn has_remote_channel_file_access_denied(&self) -> bool {
        self.remote.has_files_access_denied()
    }

    /// Completes a remote-directory navigation without treating an opaque
    /// provider channel as a local catalog key. Local drafts remain in memory
    /// for when their real local channel is selected again, while transient
    /// local overlays cannot cover the isolated remote channel page.
    fn on_remote_channel_selected(&mut self) {
        self.active_thread = None;
        self.thread_draft.clear();
        self.editing_reply = None;
        self.link_preview = None;
    }

    fn remote_channel_selection_action(&self) -> Option<TeamsAction> {
        let (team, channel) = self.remote.selected_targets()?;
        Some(TeamsAction::RemoteChannelSelected { team, channel })
    }

    /// Produces one root-channel post command from the selected remote
    /// composer's session-only draft. The provider adapter owns delivery and
    /// no local post or persistence projection is changed here.
    fn remote_channel_post_submission_action(
        &mut self,
    ) -> Result<TeamsAction, remote::RemoteChannelPostError> {
        let request = self.remote.queue_channel_post()?;
        Ok(TeamsAction::SendRemoteChannelPost {
            submission: request.submission,
            team: request.team,
            channel: request.channel,
            body: request.body,
        })
    }

    /// Produces one selected-root-post reply command from the session-only
    /// draft. The remote projection remains the sole owner of visible rows;
    /// no local thread, reply, or durable Teams state is mutated here.
    fn remote_channel_reply_submission_action(
        &mut self,
    ) -> Result<TeamsAction, remote_delivery::RemoteChannelDeliveryError> {
        let request = self.remote.queue_channel_reply()?;
        Ok(TeamsAction::SendRemoteChannelReply {
            submission: request.submission,
            team: request.team,
            channel: request.channel,
            post: request.post,
            body: request.body,
        })
    }

    /// Converts one visible remote root-post reaction request into a typed
    /// workspace action. The remote feed remains the only source of rendered
    /// reaction state; this never changes a local discussion or persistence.
    fn remote_channel_post_reaction_action(
        &mut self,
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<TeamsAction> {
        let request = self
            .remote
            .queue_channel_post_reaction(post, reaction, selected)?;
        Some(TeamsAction::SetRemoteChannelPostReaction {
            team: request.team,
            channel: request.channel,
            post: request.post,
            reaction: request.reaction,
            selected: request.selected,
        })
    }

    /// Converts one visible selected-root-post reply reaction request into a
    /// typed workspace action. The reply projection remains the only source
    /// of rendered reaction state; this never changes a local thread or the
    /// durable Teams snapshot.
    fn remote_channel_reply_reaction_action(
        &mut self,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    ) -> Option<TeamsAction> {
        let request = self
            .remote
            .queue_channel_reply_reaction(reply, reaction, selected)?;
        Some(TeamsAction::SetRemoteChannelReplyReaction {
            team: request.team,
            channel: request.channel,
            post: request.post,
            reply: request.reply,
            reaction: request.reaction,
            selected: request.selected,
        })
    }

    /// Turns a post selected from the remote root-feed into its exact
    /// selection-scoped read intent. This state transition is separate from
    /// local thread and composer actions by construction.
    fn remote_channel_post_selection_action(
        &mut self,
        post: ProviderResourceRef,
    ) -> Option<TeamsAction> {
        if !self.remote.select_post(post) {
            return None;
        }
        let (team, channel, post) = self.remote.selected_reply_targets()?;
        Some(TeamsAction::RemoteChannelPostSelected {
            team,
            channel,
            post,
        })
    }

    /// Reissues the exact same selected-root reply projection read. Unlike selecting
    /// another root post, this preserves the opaque route and does not
    /// request cancellation of same-target work.
    fn remote_channel_reply_refresh_action(&mut self) -> Option<TeamsAction> {
        if !self.remote.refresh_replies() {
            return None;
        }
        let (team, channel, post) = self.remote.selected_reply_targets()?;
        Some(TeamsAction::RemoteChannelPostSelected {
            team,
            channel,
            post,
        })
    }

    /// Selects the remote channel's metadata-only Files surface. The first
    /// visit starts an independent bounded read; later visits reuse its
    /// session-only snapshot without disturbing remote posts or replies.
    fn remote_channel_files_selection_action(&mut self) -> Option<TeamsAction> {
        if !self.remote.show_files() {
            return None;
        }
        let (team, channel) = self.remote.selected_targets()?;
        Some(TeamsAction::RemoteChannelFilesSelected { team, channel })
    }

    /// Requests the same selected channel's metadata-only files view again.
    /// It preserves opaque targets and leaves root-post/reply state intact;
    /// controller-level deduplication handles an active same-target read.
    fn remote_channel_files_refresh_action(&mut self) -> Option<TeamsAction> {
        if !self.remote.refresh_files() {
            return None;
        }
        let (team, channel) = self.remote.selected_targets()?;
        Some(TeamsAction::RemoteChannelFilesSelected { team, channel })
    }

    /// Converts only a direct selected-channel Files click into a one-shot
    /// browser handoff. The remote state rechecks the opaque target against
    /// the latest selected channel-file projection before accepting it.
    fn remote_channel_file_open_action(&mut self, file: ProviderResourceRef) {
        let _ = self.remote.request_channel_file_open(file);
    }

    #[cfg(test)]
    pub(crate) fn request_remote_channel_file_open_for_test(
        &mut self,
        file: ProviderResourceRef,
    ) -> bool {
        self.remote.request_channel_file_open(file)
    }

    /// Returns from the remote Files surface without clearing its read-only
    /// snapshot or the independent root-post/reply selection state.
    fn remote_channel_posts_surface_action(&mut self) {
        self.remote.show_posts();
    }

    #[cfg(test)]
    pub(crate) fn active_thread_key(&self) -> Option<&str> {
        self.active_thread
            .as_ref()
            .map(|thread| thread.key.as_str())
    }

    /// Restores durable local mutations while deliberately resetting search,
    /// composer, modal, and inline-thread presentation state.
    pub(crate) fn from_stored(stored: StoredTeams) -> Self {
        let StoredTeams {
            selected_channel,
            next_local_post_id,
            channels,
            teams_and_channels_open,
            communities_open,
            expanded_teams,
            joined_communities,
            ..
        } = stored;
        let channel_state = channels
            .into_iter()
            .filter_map(|(channel_key, stored_channel)| {
                channel_for_key(&channel_key)?;
                let StoredChannelState {
                    local_posts,
                    notes,
                    supplier_notes,
                    monthly_notes,
                    replies_by_post,
                    selected_reactions_by_target,
                } = stored_channel;
                let posts = local_posts
                    .into_iter()
                    .map(|post| LocalPost {
                        id: post.id,
                        body: post.body,
                    })
                    .collect::<Vec<_>>();
                let legacy_thread_replies = replies_by_post
                    .into_iter()
                    .filter(|(post_key, replies)| {
                        team_post_exists(&channel_key, &posts, post_key) && !replies.is_empty()
                    })
                    .collect();
                let legacy_reactions = selected_reactions_by_target
                    .into_iter()
                    .filter(|(target_key, selected_icons)| {
                        team_reaction_seed_for_key(&channel_key, &posts, target_key).is_some()
                            && !selected_icons.is_empty()
                    })
                    .collect();
                Some((
                    channel_key,
                    ChannelState {
                        posts,
                        notes,
                        supplier_notes,
                        monthly_notes,
                        legacy_thread_replies,
                        legacy_reactions,
                        ..ChannelState::default()
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let next_after_restored_id = channel_state
            .values()
            .flat_map(|channel| channel.posts.iter().map(|post| post.id))
            .max()
            .and_then(|id| id.checked_add(1))
            .unwrap_or(1);
        let discarded_reference_community = joined_communities
            .iter()
            .any(|name| is_reference_community(name));
        let mut state = Self {
            channel_state,
            next_local_post_id: next_local_post_id.max(next_after_restored_id).max(1),
            teams_and_channels_open,
            communities_open,
            expanded_teams: expanded_teams
                .into_iter()
                .filter(|name| team_catalog().iter().any(|team| team.name == name))
                .collect(),
            joined_communities: joined_communities
                .into_iter()
                .filter(|name| !is_reference_community(name))
                .collect(),
            ..Self::default()
        };
        if let Some(channel) = selected_channel.filter(|channel| channel_for_key(channel).is_some())
        {
            state.selected_channel = channel;
        }
        // A previously written reference-name variant is harmless to restore,
        // but must be rewritten out of the local profile on the next save.
        if discarded_reference_community {
            state.mark_persistence_dirty();
        }
        state
    }

    /// Projects only Teams-owned local mutations. Immutable catalog content
    /// and transient view state are intentionally excluded.
    pub(crate) fn stored(&self) -> StoredTeams {
        StoredTeams {
            schema_version: TEAMS_SCHEMA_VERSION,
            selected_channel: Some(self.selected_channel.clone()),
            next_local_post_id: self.next_local_post_id,
            channels: self
                .channel_state
                .iter()
                .filter_map(|(channel_key, channel)| {
                    channel_for_key(channel_key)?;
                    let stored = StoredChannelState {
                        local_posts: channel
                            .posts
                            .iter()
                            .map(|post| StoredLocalPost {
                                id: post.id,
                                body: post.body.clone(),
                            })
                            .collect(),
                        notes: channel.notes.clone(),
                        supplier_notes: channel.supplier_notes.clone(),
                        monthly_notes: channel.monthly_notes.clone(),
                        replies_by_post: channel
                            .legacy_thread_replies
                            .iter()
                            .filter(|(post_key, replies)| {
                                team_post_exists(channel_key, &channel.posts, post_key)
                                    && !replies.is_empty()
                            })
                            .map(|(post_key, replies)| (post_key.clone(), replies.clone()))
                            .collect(),
                        selected_reactions_by_target: channel
                            .legacy_reactions
                            .iter()
                            .filter(|(target_key, selected_icons)| {
                                team_reaction_seed_for_key(channel_key, &channel.posts, target_key)
                                    .is_some()
                                    && !selected_icons.is_empty()
                            })
                            .map(|(target_key, selected_icons)| {
                                (target_key.clone(), selected_icons.clone())
                            })
                            .collect(),
                    };
                    stored_channel_has_data(&stored).then_some((channel_key.clone(), stored))
                })
                .collect(),
            teams_and_channels_open: self.teams_and_channels_open,
            communities_open: self.communities_open,
            expanded_teams: self.expanded_teams.iter().cloned().collect(),
            joined_communities: self
                .joined_communities
                .iter()
                .filter(|name| !is_reference_community(name))
                .cloned()
                .collect(),
        }
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    fn mark_persistence_dirty(&mut self) {
        self.persistence_dirty = true;
    }

    /// Returns legacy reply and reaction selections recovered from
    /// `teams-v1.json`. The app-owned migration imports this immutable view
    /// into the canonical thread store before clearing it.
    pub(crate) fn legacy_discussion(&self) -> LegacyTeamsDiscussion {
        LegacyTeamsDiscussion {
            channels: self
                .channel_state
                .iter()
                .filter(|(_, state)| {
                    !state.legacy_thread_replies.is_empty() || !state.legacy_reactions.is_empty()
                })
                .map(|(channel, state)| {
                    (
                        channel.clone(),
                        LegacyTeamsChannelDiscussion {
                            replies_by_post: state.legacy_thread_replies.clone(),
                            selected_reactions_by_target: state.legacy_reactions.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Clears only the legacy discussion overlay after the app has durably
    /// written its canonical replacement. Other Teams preferences and local
    /// posts remain untouched.
    pub(crate) fn clear_legacy_discussion(&mut self) {
        let mut changed = false;
        for state in self.channel_state.values_mut() {
            changed |=
                !state.legacy_thread_replies.is_empty() || !state.legacy_reactions.is_empty();
            state.legacy_thread_replies.clear();
            state.legacy_reactions.clear();
        }
        if changed {
            self.mark_persistence_dirty();
        }
    }

    /// Resolves a legacy reply key to its stable canonical discussion ID.
    ///
    /// The coordinator uses this only while importing the pre-canonical
    /// Teams overlay. Keeping catalog and local-post validation here avoids
    /// teaching the coordinator the Teams key formats.
    pub(crate) fn legacy_thread_id_for_channel_post(
        &self,
        channel: &str,
        post_key: &str,
    ) -> Option<ThreadId> {
        let local_posts = self
            .channel_state
            .get(channel)
            .map_or(&[][..], |state| state.posts.as_slice());
        known_team_thread_id(channel, local_posts, post_key)
    }

    /// Resolves a legacy reaction key to the exact canonical thread target
    /// and its source-owned seed. `selected_icons` themselves stay in the
    /// raw legacy export so migration does not persist rendered counts.
    pub(crate) fn legacy_reaction_target(
        &self,
        channel: &str,
        target_key: &str,
    ) -> Option<(ThreadId, DiscussionTarget, &'static [ReactionSeed])> {
        let local_posts = self
            .channel_state
            .get(channel)
            .map_or(&[][..], |state| state.posts.as_slice());
        legacy_reaction_target_for_channel(channel, local_posts, target_key)
    }
}

impl TeamsState {
    pub(crate) fn has_keyboard_modal(&self) -> bool {
        self.community_picker_open
            || self.link_preview.is_some()
            || self.active_thread.is_some()
            || (!self.remote.has_selection()
                && self
                    .selected_channel_state()
                    .is_some_and(|channel| channel.composer_open))
    }

    pub(crate) fn dismiss_contextual_overlays(&mut self) {
        self.active_thread = None;
        self.editing_reply = None;
        if let Some(channel) = self.selected_channel_state_mut_if_present() {
            channel.editing_local_post = None;
        }
        self.community_picker_open = false;
        self.link_preview = None;
    }

    pub(crate) fn search_results(
        &self,
        discussions: &ThreadStore,
        query: &str,
    ) -> Vec<WorkspaceSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::new();
        for team in team_catalog() {
            for channel in team.channels {
                let label = channel_key(team, channel);
                if label.to_lowercase().contains(&query) {
                    results.push(WorkspaceSearchResult {
                        label: label.clone(),
                        detail: "Team channel".into(),
                        target: WorkspaceSearchTarget::TeamChannel(label.clone()),
                    });
                }
                for post in channel.posts {
                    let detail = if post_matches(&query, post.title, post.body) {
                        Some(format!("Post in {label}"))
                    } else if let Some(reply) = post.source_replies.iter().find(|reply| {
                        format!("{} {}", reply.author, reply.body)
                            .to_lowercase()
                            .contains(&query)
                    }) {
                        Some(format!("Reference reply by {} in {label}", reply.author))
                    } else if ThreadId::new(label.clone(), post.key).is_some_and(|thread| {
                        discussions
                            .replies(&thread)
                            .iter()
                            .any(|reply| reply.body().to_lowercase().contains(&query))
                    }) {
                        Some(format!("Your local reply in {label}"))
                    } else {
                        None
                    };
                    if let Some(detail) = detail {
                        results.push(WorkspaceSearchResult {
                            label: post.title.into(),
                            detail,
                            target: WorkspaceSearchTarget::TeamPost {
                                channel: label.clone(),
                                post_key: post.key.into(),
                            },
                        });
                    }
                }
            }
        }
        for (channel, state) in &self.channel_state {
            for post in &state.posts {
                let post_key = local_post_key(post.id);
                let detail = if post_matches(&query, "", &post.body) {
                    Some(format!("Your local post in {channel}"))
                } else if ThreadId::new(channel.clone(), post_key.clone()).is_some_and(|thread| {
                    discussions
                        .replies(&thread)
                        .iter()
                        .any(|reply| reply.body().to_lowercase().contains(&query))
                }) {
                    Some(format!("Your local reply in {channel}"))
                } else {
                    None
                };
                if let Some(detail) = detail {
                    results.push(WorkspaceSearchResult {
                        label: post_excerpt_label(&post.body),
                        detail,
                        target: WorkspaceSearchTarget::TeamPost {
                            channel: channel.clone(),
                            post_key,
                        },
                    });
                }
            }
        }
        results
    }

    fn selected_channel_state(&self) -> Option<&ChannelState> {
        self.channel_state.get(&self.selected_channel)
    }

    fn selected_channel_state_mut(&mut self) -> &mut ChannelState {
        self.channel_state
            .entry(self.selected_channel.clone())
            .or_default()
    }

    fn selected_channel_state_mut_if_present(&mut self) -> Option<&mut ChannelState> {
        self.channel_state.get_mut(&self.selected_channel)
    }

    fn open_channel_composer(&mut self) {
        let channel = self.selected_channel_state_mut();
        channel.editing_local_post = None;
        channel.composer_open = true;
        channel.composer_focus_requested = true;
    }

    fn cancel_channel_composer(&mut self) {
        let channel = self.selected_channel.clone();
        if let Some(state) = self.channel_state.get_mut(&channel) {
            state.draft.clear();
            state.composer_open = false;
            state.composer_focus_requested = false;
        }
    }

    fn submit_channel_post(&mut self) -> Result<(), TeamsSubmissionError> {
        let channel = self.selected_channel.clone();
        let id = self.next_local_post_id;
        let Some(next_local_post_id) = id.checked_add(1) else {
            return Err(TeamsSubmissionError::PostIdentifierExhausted);
        };
        let Some(channel_state) = self.channel_state.get(&channel) else {
            return Err(TeamsSubmissionError::EmptyPost);
        };
        let post = channel_state.draft.trim().to_owned();
        if post.is_empty() {
            return Err(TeamsSubmissionError::EmptyPost);
        }
        if post.len() > MAX_POST_BYTES {
            return Err(TeamsSubmissionError::PostTooLong);
        }
        if !snapshot_fits_local_store(&self.prospective_channel_post_snapshot(
            &channel,
            id,
            next_local_post_id,
            &post,
        )) {
            return Err(TeamsSubmissionError::StorageCapacity);
        }
        let channel_state = self
            .channel_state
            .get_mut(&channel)
            .expect("selected channel state was checked before a local post mutation");
        channel_state.posts.push(LocalPost { id, body: post });
        channel_state.draft.clear();
        channel_state.composer_open = false;
        channel_state.composer_focus_requested = false;
        self.next_local_post_id = next_local_post_id;
        self.mark_persistence_dirty();
        Ok(())
    }

    fn pending_channel_post(&self) -> Option<(String, String)> {
        let channel = self.selected_channel_state()?;
        let body = channel.draft.trim();
        (!body.is_empty()).then(|| (self.selected_channel.clone(), body.into()))
    }

    pub(crate) fn select_team_channel(&mut self, channel: &str) {
        let Some(team) = team_for_channel(channel) else {
            return;
        };
        if channel_for_key(channel).is_none() {
            return;
        }
        // A local channel selection must never leave a remote page active.
        // Keep the remote hierarchy itself intact so returning to it remains
        // a session-only navigation operation.
        self.remote.clear_selection();
        let mut durable_change = self.expanded_teams.insert(team.name.into());
        if !self.teams_and_channels_open {
            self.teams_and_channels_open = true;
            durable_change = true;
        }
        if self.selected_channel == channel {
            if durable_change {
                self.mark_persistence_dirty();
            }
            return;
        }
        if let Some(previous_channel) = self.selected_channel_state_mut_if_present() {
            previous_channel.editing_local_post = None;
        }
        self.selected_channel = channel.into();
        durable_change = true;
        self.team_tab = TeamTab::Posts;
        self.clear_channel_search();
        self.active_thread = None;
        self.thread_draft.clear();
        self.editing_reply = None;
        self.link_preview = None;
        if durable_change {
            self.mark_persistence_dirty();
        }
    }

    fn open_post_thread(
        &mut self,
        key: &str,
        title: &str,
        source_reply_count: usize,
        source_replies: &'static [SourceReply],
    ) {
        self.active_thread = Some(ThreadTarget {
            channel: self.selected_channel.clone(),
            key: key.into(),
            title: title.into(),
            source_reply_count,
            source_replies,
        });
        self.thread_draft.clear();
        self.editing_reply = None;
    }

    pub(crate) fn open_selected_post_thread(&mut self, key: &str) -> bool {
        let source_post = channel_for_key(&self.selected_channel)
            .and_then(|channel| channel.posts.iter().find(|post| post.key == key))
            .copied();
        if let Some(post) = source_post {
            self.show_posts_for_thread();
            self.open_post_thread(
                post.key,
                post.title,
                post.source_reply_count,
                post.source_replies,
            );
            return true;
        }

        let is_local_post = local_post_id_from_key(key).is_some_and(|id| {
            self.selected_channel_state()
                .is_some_and(|channel| channel.posts.iter().any(|post| post.id == id))
        });
        if is_local_post {
            self.show_posts_for_thread();
            self.open_post_thread(key, "New channel post", 0, &[]);
        }
        is_local_post
    }

    fn show_posts_for_thread(&mut self) {
        self.team_tab = TeamTab::Posts;
        self.clear_channel_search();
        self.link_preview = None;
    }

    fn submit_thread_reply(
        &mut self,
        discussions: &mut ThreadStore,
    ) -> Result<(), TeamsSubmissionError> {
        let Some(thread) = self.active_thread.clone() else {
            return Err(TeamsSubmissionError::NoActiveThread);
        };
        let reply = self.thread_draft.trim().to_owned();
        discussions
            .append_reply(Self::discussion_id(&thread.channel, &thread.key), &reply)
            .map_err(TeamsSubmissionError::Discussion)?;
        self.thread_draft.clear();
        Ok(())
    }

    fn begin_editing_local_reply(&mut self, thread: &ThreadTarget, reply: &LocalThreadReply) {
        self.editing_reply = Some(LocalReplyEditor {
            channel: thread.channel.clone(),
            post_key: thread.key.clone(),
            reply_id: reply.id(),
            draft: reply.body().into(),
            focus_requested: true,
        });
    }

    fn is_editing_local_reply(&self, thread: &ThreadTarget, reply_id: u64) -> bool {
        self.editing_reply.as_ref().is_some_and(|editor| {
            editor.channel == thread.channel
                && editor.post_key == thread.key
                && editor.reply_id == reply_id
        })
    }

    fn take_editing_local_reply_focus(&mut self, thread: &ThreadTarget, reply_id: u64) -> bool {
        let Some(editor) = self.editing_reply.as_mut() else {
            return false;
        };
        if editor.channel != thread.channel
            || editor.post_key != thread.key
            || editor.reply_id != reply_id
        {
            return false;
        }
        std::mem::take(&mut editor.focus_requested)
    }

    fn editing_local_reply_draft_mut(
        &mut self,
        thread: &ThreadTarget,
        reply_id: u64,
    ) -> Option<&mut String> {
        let editor = self.editing_reply.as_mut()?;
        (editor.channel == thread.channel
            && editor.post_key == thread.key
            && editor.reply_id == reply_id)
            .then_some(&mut editor.draft)
    }

    fn save_editing_local_reply(
        &mut self,
        discussions: &mut ThreadStore,
        thread: &ThreadTarget,
        reply_id: u64,
    ) -> Result<(), ThreadError> {
        let Some((editor_reply_id, draft)) = self.editing_reply.as_ref().and_then(|editor| {
            (editor.channel == thread.channel
                && editor.post_key == thread.key
                && editor.reply_id == reply_id)
                .then(|| (editor.reply_id, editor.draft.clone()))
        }) else {
            return Err(ThreadError::MissingLocalReply);
        };
        discussions.edit_reply(
            Self::discussion_id(&thread.channel, &thread.key),
            editor_reply_id,
            &draft,
        )?;
        self.editing_reply = None;
        Ok(())
    }

    fn cancel_editing_local_reply(&mut self, thread: &ThreadTarget, reply_id: u64) {
        if self.is_editing_local_reply(thread, reply_id) {
            self.editing_reply = None;
        }
    }

    fn cancel_editing_reply_for_thread(&mut self, thread: &ThreadTarget) -> bool {
        let matches = self.editing_reply.as_ref().is_some_and(|editor| {
            editor.channel == thread.channel && editor.post_key == thread.key
        });
        if matches {
            self.editing_reply = None;
        }
        matches
    }

    fn delete_local_reply(
        &mut self,
        discussions: &mut ThreadStore,
        thread: &ThreadTarget,
        reply_id: u64,
    ) -> Result<(), ThreadError> {
        discussions.delete_reply(Self::discussion_id(&thread.channel, &thread.key), reply_id)?;
        self.cancel_editing_local_reply(thread, reply_id);
        Ok(())
    }

    fn begin_editing_local_post(&mut self, id: u64) -> bool {
        let Some(post) = self
            .selected_channel_state()
            .and_then(|channel| channel.posts.iter().find(|post| post.id == id))
        else {
            return false;
        };
        self.selected_channel_state_mut().editing_local_post = Some(LocalPostEditor {
            id,
            draft: post.body.clone(),
            focus_requested: true,
        });
        true
    }

    fn is_editing_local_post(&self, id: u64) -> bool {
        self.selected_channel_state()
            .and_then(|channel| channel.editing_local_post.as_ref())
            .is_some_and(|editor| editor.id == id)
    }

    fn take_editing_local_post_focus(&mut self, id: u64) -> bool {
        self.selected_channel_state_mut_if_present()
            .and_then(|channel| channel.editing_local_post.as_mut())
            .filter(|editor| editor.id == id)
            .is_some_and(|editor| std::mem::take(&mut editor.focus_requested))
    }

    fn editing_local_post_draft_mut(&mut self, id: u64) -> Option<&mut String> {
        let editor = self
            .selected_channel_state_mut_if_present()?
            .editing_local_post
            .as_mut()?;
        (editor.id == id).then_some(&mut editor.draft)
    }

    fn cancel_editing_local_post(&mut self, id: u64) {
        if self.is_editing_local_post(id) {
            self.selected_channel_state_mut().editing_local_post = None;
        }
    }

    fn save_editing_local_post(&mut self, id: u64) -> Result<(), TeamsSubmissionError> {
        let channel = self.selected_channel.clone();
        let draft = self
            .selected_channel_state()
            .and_then(|state| state.editing_local_post.as_ref())
            .filter(|editor| editor.id == id)
            .map(|editor| editor.draft.clone())
            .ok_or(TeamsSubmissionError::LocalPostMissing)?;
        let body = draft.trim();
        if body.is_empty() {
            return Err(TeamsSubmissionError::EmptyPost);
        }
        if body.len() > MAX_POST_BYTES {
            return Err(TeamsSubmissionError::PostTooLong);
        }
        let snapshot = self
            .prospective_local_post_edit_snapshot(&channel, id, body)
            .ok_or(TeamsSubmissionError::LocalPostMissing)?;
        if !snapshot_fits_local_store(&snapshot) {
            return Err(TeamsSubmissionError::StorageCapacity);
        }
        let changed = {
            let post = self
                .selected_channel_state_mut()
                .posts
                .iter_mut()
                .find(|post| post.id == id)
                .ok_or(TeamsSubmissionError::LocalPostMissing)?;
            if post.body == body {
                false
            } else {
                post.body = body.into();
                true
            }
        };
        self.selected_channel_state_mut().editing_local_post = None;
        if changed {
            self.mark_persistence_dirty();
        }
        Ok(())
    }

    fn delete_local_post(
        &mut self,
        discussions: &mut ThreadStore,
        id: u64,
    ) -> Result<(), TeamsSubmissionError> {
        let channel = self.selected_channel.clone();
        let key = local_post_key(id);
        let snapshot = self
            .prospective_local_post_delete_snapshot(&channel, id, &key)
            .ok_or(TeamsSubmissionError::LocalPostMissing)?;
        if !snapshot_fits_local_store(&snapshot) {
            return Err(TeamsSubmissionError::StorageCapacity);
        }
        let channel_state = self.selected_channel_state_mut();
        let Some(index) = channel_state.posts.iter().position(|post| post.id == id) else {
            return Err(TeamsSubmissionError::LocalPostMissing);
        };
        let post = channel_state.posts.remove(index);
        let legacy_replies = channel_state.legacy_thread_replies.remove(&key);
        let legacy_reactions = channel_state.legacy_reactions.remove(&key);
        channel_state.editing_local_post = None;
        let thread_id = Self::discussion_id(&channel, &key);
        let thread = discussions.detach_thread(&thread_id);
        self.undo_post = Some(DeletedLocalPost {
            channel: channel.clone(),
            index,
            post,
            key: key.clone(),
            legacy_replies,
            legacy_reactions,
            thread,
        });
        if self
            .active_thread
            .as_ref()
            .is_some_and(|active| active.channel == channel && active.key == key)
        {
            self.active_thread = None;
            self.thread_draft.clear();
            self.editing_reply = None;
        }
        self.mark_persistence_dirty();
        Ok(())
    }

    /// Restores the last deleted local channel post, its replies and its
    /// reactions, then persists them again. The buffer is consumed either way.
    pub(crate) fn undo_delete_local_post(&mut self, discussions: &mut ThreadStore) -> bool {
        self.try_undo_delete_local_post(discussions).is_ok()
    }

    fn try_undo_delete_local_post(
        &mut self,
        discussions: &mut ThreadStore,
    ) -> Result<(), TeamsSubmissionError> {
        let Some(deleted) = self.undo_post.take() else {
            return Err(TeamsSubmissionError::LocalPostMissing);
        };
        let post_id = deleted.post.id;
        let channel_state = self
            .channel_state
            .entry(deleted.channel.clone())
            .or_default();
        if channel_state.posts.iter().any(|post| post.id == post_id) {
            return Err(TeamsSubmissionError::LocalPostMissing);
        }
        let index = deleted.index.min(channel_state.posts.len());
        channel_state.posts.insert(index, deleted.post);
        if let Some(replies) = deleted.legacy_replies {
            channel_state
                .legacy_thread_replies
                .insert(deleted.key.clone(), replies);
        }
        if let Some(reactions) = deleted.legacy_reactions {
            channel_state
                .legacy_reactions
                .insert(deleted.key.clone(), reactions);
        }
        let mut fits = snapshot_fits_local_store(&self.stored());
        if fits && let Some(thread) = deleted.thread {
            fits = discussions.reattach_thread(thread).is_ok();
        }
        if !fits {
            // Roll the post back out: nothing was persisted for it yet.
            if let Some(channel_state) = self.channel_state.get_mut(&deleted.channel) {
                channel_state.posts.retain(|post| post.id != post_id);
                channel_state.legacy_thread_replies.remove(&deleted.key);
                channel_state.legacy_reactions.remove(&deleted.key);
            }
            return Err(TeamsSubmissionError::StorageCapacity);
        }
        self.mark_persistence_dirty();
        Ok(())
    }

    fn prospective_channel_post_snapshot(
        &self,
        channel: &str,
        id: u64,
        next_local_post_id: u64,
        body: &str,
    ) -> StoredTeams {
        let mut snapshot = self.stored();
        snapshot.next_local_post_id = next_local_post_id;
        snapshot
            .channels
            .entry(channel.into())
            .or_default()
            .local_posts
            .push(StoredLocalPost {
                id,
                body: body.into(),
            });
        snapshot
    }

    fn prospective_local_post_edit_snapshot(
        &self,
        channel: &str,
        id: u64,
        body: &str,
    ) -> Option<StoredTeams> {
        let mut snapshot = self.stored();
        let post = snapshot
            .channels
            .get_mut(channel)?
            .local_posts
            .iter_mut()
            .find(|post| post.id == id)?;
        post.body = body.into();
        Some(snapshot)
    }

    fn prospective_local_post_delete_snapshot(
        &self,
        channel: &str,
        id: u64,
        key: &str,
    ) -> Option<StoredTeams> {
        let mut snapshot = self.stored();
        let state = snapshot.channels.get_mut(channel)?;
        let before = state.local_posts.len();
        state.local_posts.retain(|post| post.id != id);
        (state.local_posts.len() != before).then_some(())?;
        state.replies_by_post.remove(key);
        state.selected_reactions_by_target.remove(key);
        Some(snapshot)
    }

    fn discussion_id(channel: &str, post_key: &str) -> ThreadId {
        ThreadId::new(channel, post_key)
            .expect("Teams only opens catalogued channels and stable source or local post keys")
    }

    fn thread_replies<'a>(
        &self,
        discussions: &'a ThreadStore,
        thread: &ThreadTarget,
    ) -> &'a [LocalThreadReply] {
        discussions.replies(&Self::discussion_id(&thread.channel, &thread.key))
    }

    #[cfg(test)]
    fn selected_thread_reply_count(&self, discussions: &ThreadStore, key: &str) -> usize {
        discussions
            .replies(&Self::discussion_id(&self.selected_channel, key))
            .len()
    }

    fn reactions_for_post(
        &self,
        discussions: &ThreadStore,
        post_key: &str,
        target: &DiscussionTarget,
        seed: &[ReactionSeed],
    ) -> ReactionSet {
        Self::reactions_for_discussion(
            discussions,
            &Self::discussion_id(&self.selected_channel, post_key),
            target,
            seed,
        )
    }

    /// Same projection as [`Self::reactions_for_post`] for a caller that has
    /// already built the post's `ThreadId`, so a per-frame render builds one id
    /// per post instead of one for the reply count and one for the reactions.
    fn reactions_for_discussion(
        discussions: &ThreadStore,
        id: &ThreadId,
        target: &DiscussionTarget,
        seed: &[ReactionSeed],
    ) -> ReactionSet {
        discussions.reactions(id, target, seed)
    }

    fn toggle_post_reaction(
        &self,
        discussions: &mut ThreadStore,
        post_key: &str,
        target: DiscussionTarget,
        seed: &[ReactionSeed],
        icon: &str,
    ) -> Result<bool, ThreadError> {
        discussions.toggle_reaction(
            Self::discussion_id(&self.selected_channel, post_key),
            target,
            seed,
            icon,
        )
    }

    fn show_all_teams(&mut self) {
        self.clear_team_filter();
        let expanded_teams = team_catalog().iter().map(|team| team.name.into()).collect();
        if !self.teams_and_channels_open || self.expanded_teams != expanded_teams {
            self.teams_and_channels_open = true;
            self.expanded_teams = expanded_teams;
            self.mark_persistence_dirty();
        }
    }

    fn clear_team_filter(&mut self) {
        self.team_filter_query.clear();
        self.team_filter_open = false;
    }

    fn toggle_team_filter(&mut self) {
        if self.team_filter_open {
            self.clear_team_filter();
        } else {
            self.team_filter_open = true;
            if !self.teams_and_channels_open {
                self.teams_and_channels_open = true;
                self.mark_persistence_dirty();
            }
        }
    }

    fn clear_channel_search(&mut self) {
        self.team_search_query.clear();
        self.team_search_open = false;
    }

    fn toggle_channel_search(&mut self) {
        if self.team_search_open {
            self.clear_channel_search();
        } else {
            self.team_search_open = true;
        }
    }

    fn is_team_expanded(&self, team_name: &str) -> bool {
        self.expanded_teams.contains(team_name)
    }

    fn toggle_team_expansion(&mut self, team_name: &str) {
        if !self.expanded_teams.remove(team_name) {
            self.expanded_teams.insert(team_name.into());
        }
        self.mark_persistence_dirty();
    }

    fn toggle_teams_and_channels(&mut self) {
        self.teams_and_channels_open = !self.teams_and_channels_open;
        self.mark_persistence_dirty();
    }

    fn toggle_communities(&mut self) {
        self.communities_open = !self.communities_open;
        self.mark_persistence_dirty();
    }

    fn open_community_picker(&mut self) {
        self.link_preview = None;
        self.community_picker_open = true;
    }

    fn open_link_preview(&mut self, label: &str) {
        self.community_picker_open = false;
        self.link_preview =
            Some(LinkPreview::from_text(label).unwrap_or_else(|| LinkPreview::resource(label)));
    }

    fn join_community(&mut self, name: &str) -> bool {
        if is_reference_community(name) {
            return false;
        }
        if self
            .joined_communities
            .iter()
            .any(|community| community == name)
        {
            return false;
        }
        self.joined_communities.push(name.into());
        self.communities_open = true;
        self.mark_persistence_dirty();
        true
    }

    /// Removes one profile-scoped preview membership.
    ///
    /// This deliberately changes only the Teams-local snapshot. It does not
    /// leave, delete, or otherwise mutate a provider-backed Teams community.
    fn remove_joined_community(&mut self, name: &str) -> bool {
        if is_reference_community(name) {
            return false;
        }
        let Some(index) = self
            .joined_communities
            .iter()
            .position(|community| community == name)
        else {
            return false;
        };
        self.joined_communities.remove(index);
        self.mark_persistence_dirty();
        true
    }
}

#[derive(Default)]
struct ChannelState {
    posts: Vec<LocalPost>,
    notes: String,
    supplier_notes: String,
    monthly_notes: String,
    draft: String,
    composer_open: bool,
    composer_focus_requested: bool,
    editing_local_post: Option<LocalPostEditor>,
    // Retained only until the app-owned migration durably imports this
    // teams-v1 overlay into the canonical ThreadStore.
    legacy_thread_replies: BTreeMap<String, Vec<String>>,
    legacy_reactions: BTreeMap<String, Vec<String>>,
}

/// Presentation state for editing exactly one canonical local reply.
///
/// The text itself remains in `ThreadStore`; Teams owns only the temporary
/// draft and focus request so Activity immediately reflects a saved edit.
struct LocalReplyEditor {
    channel: String,
    post_key: String,
    reply_id: u64,
    draft: String,
    focus_requested: bool,
}

#[derive(Default)]
struct LocalPostEditor {
    id: u64,
    draft: String,
    focus_requested: bool,
}

/// The pre-canonical mutable discussion projection from `teams-v1.json`.
///
/// This is intentionally raw (selected reaction icons rather than rendered
/// `ReactionSet`s) so migration can preserve the exact user selections
/// without treating catalog counts as durable data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LegacyTeamsDiscussion {
    pub(crate) channels: BTreeMap<String, LegacyTeamsChannelDiscussion>,
}

impl LegacyTeamsDiscussion {
    pub(crate) fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LegacyTeamsChannelDiscussion {
    pub(crate) replies_by_post: BTreeMap<String, Vec<String>>,
    pub(crate) selected_reactions_by_target: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalPost {
    id: u64,
    body: String,
}

#[derive(Clone)]
struct ThreadTarget {
    channel: String,
    key: String,
    title: String,
    source_reply_count: usize,
    source_replies: &'static [SourceReply],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TeamTab {
    Posts,
    Shared,
    Notes,
    Suppliers,
    Monthly,
}

impl TeamTab {
    const fn label(self) -> &'static str {
        match self {
            Self::Posts => "Posts",
            Self::Shared => "Shared",
            Self::Notes => "Notes",
            Self::Suppliers => "Supplier contacts",
            Self::Monthly => "Monthly · Workshop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TeamsAction {
    StartMeeting,
    OpenSharedPreview(String),
    ChannelPostSubmitted {
        channel: String,
        body: String,
    },
    /// A text-only root-channel post for one exact opaque Microsoft 365
    /// route. Delivery is reconciled through the submission/intent lifecycle
    /// instead of an optimistic local feed mutation.
    SendRemoteChannelPost {
        submission: RemoteChannelPostSubmissionId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        body: String,
    },
    /// A text-only reply to one exact opaque Microsoft 365 root post. Like a
    /// root post, delivery is resolved through the submission/intent
    /// lifecycle and never through an optimistic local row.
    SendRemoteChannelReply {
        submission: RemoteChannelReplySubmissionId,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        body: String,
    },
    /// Changes the current user's bounded-palette reaction state for one exact
    /// remote root post. The feature locks the exact opaque route and typed
    /// kind until a fresh matching root-post projection arrives; it never
    /// creates an optimistic local row or durable reaction overlay.
    SetRemoteChannelPostReaction {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    /// Changes the current user's bounded-palette reaction state for one exact
    /// remote reply beneath one exact root post. The feature locks the
    /// complete opaque route and typed kind until a fresh matching selected
    /// reply projection arrives; it never creates an optimistic local row or
    /// durable reaction overlay.
    SetRemoteChannelReplyReaction {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
        reply: ProviderResourceRef,
        reaction: ReactionKind,
        selected: bool,
    },
    /// A selection-scoped, read-only remote query. The app routes it to the
    /// transport controller; it is never a local post or persistence action.
    RemoteChannelSelected {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
    /// A selection-scoped reply projection read for one opaque root post.
    /// It is intentionally separate from the text-only reply mutation,
    /// local thread, and reaction actions.
    RemoteChannelPostSelected {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    },
    /// A selection-scoped, read-only channel-folder metadata query. It is
    /// separate from the global OneDrive view and never becomes a local file
    /// action or provider mutation.
    RemoteChannelFilesSelected {
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    },
}

/// Why the local Teams projection did not accept a new post or thread reply.
///
/// The validation mirrors the durable store before mutating any user-visible
/// state, so a successful action can be saved by the normal lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TeamsSubmissionError {
    EmptyPost,
    PostTooLong,
    NoActiveThread,
    LocalPostMissing,
    PostIdentifierExhausted,
    StorageCapacity,
    Discussion(ThreadError),
}

impl TeamsSubmissionError {
    fn message(self) -> String {
        match self {
            Self::EmptyPost => "Write something before posting.".into(),
            Self::PostTooLong => format!(
                "Channel posts can contain at most {} KiB of text.",
                MAX_POST_BYTES / 1024
            ),
            Self::NoActiveThread => "Open a thread before replying.".into(),
            Self::LocalPostMissing => "That local channel post is no longer available.".into(),
            Self::PostIdentifierExhausted | Self::StorageCapacity => {
                "This local Teams profile is full; remove local content before adding more.".into()
            }
            Self::Discussion(error) => error.to_string(),
        }
    }
}

fn post_matches(query: &str, title: &str, body: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if title.is_empty() {
        return body.to_lowercase().contains(query);
    }
    format!("{title} {body}").to_lowercase().contains(query)
}

const POST_EXCERPT_CHARS: usize = 48;

/// Search-result label for a local post: the first non-empty body line,
/// shortened, so results are identified by what the post says.
fn post_excerpt_label(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("New channel post");
    let mut chars = line.chars();
    let excerpt: String = chars.by_ref().take(POST_EXCERPT_CHARS).collect();
    if chars.next().is_some() {
        format!("{}…", excerpt.trim_end())
    } else {
        excerpt
    }
}

fn known_team_thread_id(
    channel_key: &str,
    local_posts: &[LocalPost],
    post_key: &str,
) -> Option<ThreadId> {
    team_post_exists(channel_key, local_posts, post_key)
        .then(|| ThreadId::new(channel_key, post_key))
        .flatten()
}

fn team_post_exists(channel_key: &str, local_posts: &[LocalPost], key: &str) -> bool {
    channel_for_key(channel_key)
        .is_some_and(|channel| channel.posts.iter().any(|post| post.key == key))
        || local_post_id_from_key(key)
            .is_some_and(|id| local_posts.iter().any(|post| post.id == id))
}

fn legacy_reaction_target_for_channel(
    channel_key: &str,
    local_posts: &[LocalPost],
    target_key: &str,
) -> Option<(ThreadId, DiscussionTarget, &'static [ReactionSeed])> {
    let channel = channel_for_key(channel_key)?;
    if let Some(post) = channel.posts.iter().find(|post| post.key == target_key) {
        return Some((
            known_team_thread_id(channel_key, local_posts, post.key)?,
            DiscussionTarget::Post,
            post.reactions,
        ));
    }
    if local_post_id_from_key(target_key)
        .is_some_and(|id| local_posts.iter().any(|post| post.id == id))
    {
        return Some((
            known_team_thread_id(channel_key, local_posts, target_key)?,
            DiscussionTarget::Post,
            NO_REACTIONS,
        ));
    }
    channel.posts.iter().find_map(|post| {
        post.source_replies
            .iter()
            .find(|reply| source_reply_reaction_key(post.key, reply) == target_key)
            .and_then(|reply| {
                Some((
                    known_team_thread_id(channel_key, local_posts, post.key)?,
                    DiscussionTarget::source_reply(reply.key)?,
                    reply.reactions,
                ))
            })
    })
}

fn team_reaction_seed_for_key(
    channel_key: &str,
    local_posts: &[LocalPost],
    target_key: &str,
) -> Option<&'static [ReactionSeed]> {
    legacy_reaction_target_for_channel(channel_key, local_posts, target_key)
        .map(|(_, _, seed)| seed)
}

fn stored_channel_has_data(channel: &StoredChannelState) -> bool {
    !channel.local_posts.is_empty()
        || !channel.notes.is_empty()
        || !channel.supplier_notes.is_empty()
        || !channel.monthly_notes.is_empty()
        || !channel.replies_by_post.is_empty()
        || !channel.selected_reactions_by_target.is_empty()
}

fn team_matches_filter(filter: &str, team: &TeamDefinition) -> bool {
    filter.is_empty()
        || format!(
            "{} {}",
            team.name,
            team.channels
                .iter()
                .map(|channel| channel.name)
                .collect::<Vec<_>>()
                .join(" ")
        )
        .to_lowercase()
        .contains(filter)
}

#[cfg(test)]
mod tests;
