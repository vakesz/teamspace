//! Canonical local discussion overlays shared by Activity and Teams.
//!
//! Immutable source posts and reference replies remain feature-owned. This
//! module owns only the local user's durable additions and reaction choices,
//! keyed by a stable channel/post identity. Keeping that mutable overlay here
//! prevents two surfaces from silently creating different local histories for
//! the same discussion.

mod persistence;
#[cfg(feature = "fuzzing")]
pub(crate) use persistence::fuzz_snapshot;

#[allow(unused_imports)] // Consumed by the app-owned persistence lifecycle.
pub(crate) use self::persistence::{LocalThreadStore, StoredThreads};
use self::persistence::{
    MAX_SNAPSHOT_BYTES, StoredLocalThreadReply, StoredReactionSelection, StoredThread,
    THREADS_SCHEMA_VERSION,
};
use crate::ui::reactions::{ReactionSeed, ReactionSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(super) const MAX_THREADS: usize = 10_000;
pub(super) const MAX_REPLIES_PER_THREAD: usize = 10_000;
pub(super) const MAX_REACTION_TARGETS_PER_THREAD: usize = 10_000;
pub(super) const MAX_SELECTED_REACTIONS_PER_TARGET: usize = 16;
pub(super) const MAX_THREAD_KEY_BYTES: usize = 512;
pub(super) const MAX_REPLY_BYTES: usize = 64 * 1024;
pub(super) const MAX_REACTION_ICON_BYTES: usize = 64;

/// Stable identity for a discussion rooted at one channel post.
///
/// It is deliberately independent from catalog positions and display text, so
/// a source list can be reordered without changing local replies or reactions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct ThreadId {
    channel: String,
    post_key: String,
}

impl ThreadId {
    pub(crate) fn new(channel: impl Into<String>, post_key: impl Into<String>) -> Option<Self> {
        let id = Self {
            channel: channel.into(),
            post_key: post_key.into(),
        };
        id.is_valid().then_some(id)
    }

    pub(crate) fn channel(&self) -> &str {
        &self.channel
    }

    pub(crate) fn post_key(&self) -> &str {
        &self.post_key
    }

    pub(super) fn is_valid(&self) -> bool {
        valid_key(&self.channel) && valid_key(&self.post_key)
    }
}

/// An addressable mutable reaction target inside a thread.
///
/// Source-reply identifiers are owned by reference catalog data. Local reply
/// identifiers are monotonic and owned by this module, never by a UI index.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThreadTarget {
    Post,
    SourceReply { reply_key: String },
    LocalReply { reply_id: u64 },
}

impl ThreadTarget {
    pub(crate) fn source_reply(reply_key: impl Into<String>) -> Option<Self> {
        let target = Self::SourceReply {
            reply_key: reply_key.into(),
        };
        target.is_well_formed().then_some(target)
    }

    pub(crate) fn local_reply(reply_id: u64) -> Option<Self> {
        (reply_id != 0).then_some(Self::LocalReply { reply_id })
    }

    pub(super) fn is_well_formed(&self) -> bool {
        match self {
            Self::Post => true,
            Self::SourceReply { reply_key } => valid_key(reply_key),
            Self::LocalReply { reply_id } => *reply_id != 0,
        }
    }
}

/// A durable reply created locally. Its ID survives reorderings and restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalThreadReply {
    id: u64,
    body: String,
}

impl LocalThreadReply {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

/// A transient accepted mutation. The app coordinator may map an addressable
/// Teams thread to a transport command, while Activity-only overlays remain
/// local. Changes are never persisted or replayed after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadChange {
    ReplyAdded {
        thread: ThreadId,
        reply: LocalThreadReply,
    },
    ReplyEdited {
        thread: ThreadId,
        reply: LocalThreadReply,
    },
    ReplyDeleted {
        thread: ThreadId,
        reply_id: u64,
    },
    ReactionChanged {
        thread: ThreadId,
        target: ThreadTarget,
        reaction: String,
        selected: bool,
    },
}

/// A migration-only mutation recovered from an older feature-owned snapshot.
///
/// The batch importer applies these atomically and intentionally emits no
/// [`ThreadChange`]: old local state was already accepted before the canonical
/// store existed and must not be replayed to a transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LegacyThreadMutation {
    Reply {
        thread: ThreadId,
        body: String,
    },
    SelectedReactions {
        thread: ThreadId,
        target: ThreadTarget,
        selected_icons: Vec<String>,
    },
}

/// A whole discussion removed from the store, held only so an Undo can put it
/// back. Opaque outside this module.
pub(crate) struct DetachedThread {
    id: ThreadId,
    state: ThreadState,
}

#[derive(Clone, Default)]
struct ThreadState {
    replies: Vec<LocalThreadReply>,
    selected_reactions: BTreeMap<ThreadTarget, Vec<String>>,
}

/// The deep module behind the shared Activity/Teams discussion overlay.
///
/// Callers only need to construct a [`ThreadId`], render replies/reactions
/// returned here, and submit accepted mutations. Validation, stable IDs,
/// selected-reaction projection, change tracking, and durable snapshots stay
/// behind this interface.
pub(crate) struct ThreadStore {
    threads: BTreeMap<ThreadId, ThreadState>,
    next_local_reply_id: u64,
    activity_v1_imported: bool,
    teams_v1_imported: bool,
    pending_changes: Vec<ThreadChange>,
    persistence_dirty: bool,
}

impl Default for ThreadStore {
    fn default() -> Self {
        Self {
            threads: BTreeMap::new(),
            next_local_reply_id: 1,
            activity_v1_imported: false,
            teams_v1_imported: false,
            pending_changes: Vec::new(),
            persistence_dirty: false,
        }
    }
}

impl ThreadStore {
    /// Restores only the canonical local overlay. Feature view state, drafts,
    /// reference reply text, and pending transport changes remain transient.
    pub(crate) fn from_stored(stored: StoredThreads) -> Self {
        let StoredThreads {
            next_local_reply_id,
            activity_v1_imported,
            teams_v1_imported,
            threads,
            ..
        } = stored;
        let threads = threads
            .into_iter()
            .map(|stored_thread| {
                let id = ThreadId {
                    channel: stored_thread.channel,
                    post_key: stored_thread.post_key,
                };
                let replies = stored_thread
                    .replies
                    .into_iter()
                    .map(|reply| LocalThreadReply {
                        id: reply.id,
                        body: reply.body,
                    })
                    .collect();
                let selected_reactions = stored_thread
                    .selected_reactions
                    .into_iter()
                    .map(|selection| (selection.target, selection.selected_icons))
                    .collect();
                (
                    id,
                    ThreadState {
                        replies,
                        selected_reactions,
                    },
                )
            })
            .collect();
        let mut restored = Self {
            threads,
            next_local_reply_id,
            activity_v1_imported,
            teams_v1_imported,
            ..Self::default()
        };
        restored.mark_persisted();
        restored
    }

    /// Captures only local replies and the current user's reaction selections.
    pub(crate) fn stored(&self) -> StoredThreads {
        Self::stored_from_parts(
            &self.threads,
            self.next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
        )
    }

    fn stored_from_parts(
        threads: &BTreeMap<ThreadId, ThreadState>,
        next_local_reply_id: u64,
        activity_v1_imported: bool,
        teams_v1_imported: bool,
    ) -> StoredThreads {
        StoredThreads {
            schema_version: THREADS_SCHEMA_VERSION,
            next_local_reply_id,
            activity_v1_imported,
            teams_v1_imported,
            threads: threads
                .iter()
                .filter(|(_, state)| {
                    !state.replies.is_empty() || !state.selected_reactions.is_empty()
                })
                .map(|(id, state)| StoredThread {
                    channel: id.channel().into(),
                    post_key: id.post_key().into(),
                    replies: state
                        .replies
                        .iter()
                        .map(|reply| StoredLocalThreadReply {
                            id: reply.id,
                            body: reply.body.clone(),
                        })
                        .collect(),
                    selected_reactions: state
                        .selected_reactions
                        .iter()
                        .filter(|(_, icons)| !icons.is_empty())
                        .map(|(target, selected_icons)| StoredReactionSelection {
                            target: target.clone(),
                            selected_icons: selected_icons.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// The storage adapter is deliberately strict about its byte cap. Rather
    /// than accept a mutation that would later fail its debounced save, each
    /// local write serializes a prospective snapshot before committing it.
    fn snapshot_fits(
        threads: &BTreeMap<ThreadId, ThreadState>,
        next_local_reply_id: u64,
        activity_v1_imported: bool,
        teams_v1_imported: bool,
        limit: usize,
    ) -> bool {
        serde_json::to_vec_pretty(&Self::stored_from_parts(
            threads,
            next_local_reply_id,
            activity_v1_imported,
            teams_v1_imported,
        ))
        .is_ok_and(|bytes| bytes.len() <= limit)
    }

    pub(crate) const fn is_persistence_dirty(&self) -> bool {
        self.persistence_dirty
    }

    pub(crate) fn mark_persisted(&mut self) {
        self.persistence_dirty = false;
    }

    /// Whether the legacy Activity v1 overlay is already represented in this
    /// snapshot. The caller must only clear its legacy source after this
    /// marker and imported data have been successfully saved together.
    pub(crate) const fn activity_v1_imported(&self) -> bool {
        self.activity_v1_imported
    }

    /// Whether the legacy Teams v1 overlay is already represented in this
    /// snapshot. See [`Self::activity_v1_imported`] for save ordering.
    pub(crate) const fn teams_v1_imported(&self) -> bool {
        self.teams_v1_imported
    }

    pub(crate) fn mark_activity_v1_imported(&mut self) -> Result<(), ThreadError> {
        if !self.activity_v1_imported {
            if !Self::snapshot_fits(
                &self.threads,
                self.next_local_reply_id,
                true,
                self.teams_v1_imported,
                MAX_SNAPSHOT_BYTES,
            ) {
                return Err(ThreadError::SnapshotCapacity);
            }
            self.activity_v1_imported = true;
            self.persistence_dirty = true;
        }
        Ok(())
    }

    pub(crate) fn mark_teams_v1_imported(&mut self) -> Result<(), ThreadError> {
        if !self.teams_v1_imported {
            if !Self::snapshot_fits(
                &self.threads,
                self.next_local_reply_id,
                self.activity_v1_imported,
                true,
                MAX_SNAPSHOT_BYTES,
            ) {
                return Err(ThreadError::SnapshotCapacity);
            }
            self.teams_v1_imported = true;
            self.persistence_dirty = true;
        }
        Ok(())
    }

    /// Returns local replies for one exact channel post.
    pub(crate) fn replies(&self, thread: &ThreadId) -> &[LocalThreadReply] {
        self.threads
            .get(thread)
            .map_or(&[], |state| state.replies.as_slice())
    }

    /// Rebuilds a reaction set from current reference counts plus durable local
    /// selections. Reference counts are intentionally not persisted here.
    pub(crate) fn reactions(
        &self,
        thread: &ThreadId,
        target: &ThreadTarget,
        seed: &[ReactionSeed],
    ) -> ReactionSet {
        let selected_icons = self
            .threads
            .get(thread)
            .and_then(|state| state.selected_reactions.get(target))
            .map_or(&[][..], Vec::as_slice);
        ReactionSet::from_seed_with_selected(seed, selected_icons)
    }

    /// Appends a locally authored reply after fully validating it. The reply
    /// receives a monotonic ID before any view can observe it.
    pub(crate) fn append_reply(
        &mut self,
        thread: ThreadId,
        body: &str,
    ) -> Result<LocalThreadReply, ThreadError> {
        self.append_reply_with_snapshot_limit(thread, body, MAX_SNAPSHOT_BYTES)
    }

    fn append_reply_with_snapshot_limit(
        &mut self,
        thread: ThreadId,
        body: &str,
        snapshot_limit: usize,
    ) -> Result<LocalThreadReply, ThreadError> {
        if !thread.is_valid() {
            return Err(ThreadError::InvalidThread);
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(ThreadError::EmptyReply);
        }
        if !valid_reply(body) {
            return Err(ThreadError::InvalidReply);
        }
        let reply_count = self
            .threads
            .get(&thread)
            .map_or(0, |state| state.replies.len());
        if reply_count >= MAX_REPLIES_PER_THREAD {
            return Err(ThreadError::ReplyCapacity);
        }
        if !self.threads.contains_key(&thread) && self.threads.len() >= MAX_THREADS {
            return Err(ThreadError::ThreadCapacity);
        }
        let id = self.next_local_reply_id;
        let Some(next_local_reply_id) = id.checked_add(1) else {
            return Err(ThreadError::ReplyIdExhausted);
        };
        let reply = LocalThreadReply {
            id,
            body: body.into(),
        };
        let mut threads = self.threads.clone();
        threads
            .entry(thread.clone())
            .or_default()
            .replies
            .push(reply.clone());
        if !Self::snapshot_fits(
            &threads,
            next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
            snapshot_limit,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.next_local_reply_id = next_local_reply_id;
        self.persistence_dirty = true;
        self.pending_changes.push(ThreadChange::ReplyAdded {
            thread,
            reply: reply.clone(),
        });
        Ok(reply)
    }

    /// Replaces the body of one locally authored reply without changing its
    /// stable ID or its local-reaction targets. Source replies are never
    /// represented here and therefore cannot be edited through this API.
    pub(crate) fn edit_reply(
        &mut self,
        thread: ThreadId,
        reply_id: u64,
        body: &str,
    ) -> Result<LocalThreadReply, ThreadError> {
        self.edit_reply_with_snapshot_limit(thread, reply_id, body, MAX_SNAPSHOT_BYTES)
    }

    fn edit_reply_with_snapshot_limit(
        &mut self,
        thread: ThreadId,
        reply_id: u64,
        body: &str,
        snapshot_limit: usize,
    ) -> Result<LocalThreadReply, ThreadError> {
        if !thread.is_valid() {
            return Err(ThreadError::InvalidThread);
        }
        if reply_id == 0 {
            return Err(ThreadError::MissingLocalReply);
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(ThreadError::EmptyReply);
        }
        if !valid_reply(body) {
            return Err(ThreadError::InvalidReply);
        }

        let mut threads = self.threads.clone();
        let reply = threads
            .get_mut(&thread)
            .and_then(|state| state.replies.iter_mut().find(|reply| reply.id == reply_id))
            .ok_or(ThreadError::MissingLocalReply)?;
        if reply.body == body {
            return Ok(reply.clone());
        }
        reply.body = body.into();
        let updated = reply.clone();
        if !Self::snapshot_fits(
            &threads,
            self.next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
            snapshot_limit,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.persistence_dirty = true;
        self.pending_changes.push(ThreadChange::ReplyEdited {
            thread,
            reply: updated.clone(),
        });
        Ok(updated)
    }

    /// Deletes one locally authored reply and only the reactions whose target
    /// is that reply. Other post and source-reply reaction selections remain
    /// intact. IDs are intentionally never reused after deletion.
    pub(crate) fn delete_reply(
        &mut self,
        thread: ThreadId,
        reply_id: u64,
    ) -> Result<(), ThreadError> {
        self.delete_reply_with_snapshot_limit(thread, reply_id, MAX_SNAPSHOT_BYTES)
    }

    fn delete_reply_with_snapshot_limit(
        &mut self,
        thread: ThreadId,
        reply_id: u64,
        snapshot_limit: usize,
    ) -> Result<(), ThreadError> {
        if !thread.is_valid() {
            return Err(ThreadError::InvalidThread);
        }
        if reply_id == 0 {
            return Err(ThreadError::MissingLocalReply);
        }

        let mut threads = self.threads.clone();
        let state = threads
            .get_mut(&thread)
            .ok_or(ThreadError::MissingLocalReply)?;
        let Some(index) = state.replies.iter().position(|reply| reply.id == reply_id) else {
            return Err(ThreadError::MissingLocalReply);
        };
        state.replies.remove(index);
        state
            .selected_reactions
            .remove(&ThreadTarget::LocalReply { reply_id });
        if state.replies.is_empty() && state.selected_reactions.is_empty() {
            threads.remove(&thread);
        }
        if !Self::snapshot_fits(
            &threads,
            self.next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
            snapshot_limit,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.persistence_dirty = true;
        self.pending_changes
            .push(ThreadChange::ReplyDeleted { thread, reply_id });
        Ok(())
    }

    /// Removes every local overlay record for one discussion. Teams uses this
    /// when its locally authored root post is deleted. The method never
    /// affects another channel or post sharing similar display text. It hands
    /// the removed discussion back so a delete can be undone with
    /// [`Self::reattach_thread`]. Callers that do not undo simply drop it.
    pub(crate) fn detach_thread(&mut self, thread: &ThreadId) -> Option<DetachedThread> {
        let state = self.threads.remove(thread)?;
        self.persistence_dirty = true;
        self.pending_changes.retain(|change| match change {
            ThreadChange::ReplyAdded {
                thread: changed_thread,
                ..
            }
            | ThreadChange::ReplyEdited {
                thread: changed_thread,
                ..
            }
            | ThreadChange::ReplyDeleted {
                thread: changed_thread,
                ..
            }
            | ThreadChange::ReactionChanged {
                thread: changed_thread,
                ..
            } => changed_thread != thread,
        });
        Some(DetachedThread {
            id: thread.clone(),
            state,
        })
    }

    /// Puts a detached discussion back, unless that discussion has been
    /// recreated meanwhile or restoring it would not fit the profile.
    pub(crate) fn reattach_thread(&mut self, detached: DetachedThread) -> Result<(), ThreadError> {
        if self.threads.contains_key(&detached.id) {
            return Err(ThreadError::InvalidThread);
        }
        let mut threads = self.threads.clone();
        threads.insert(detached.id, detached.state);
        if !Self::snapshot_fits(
            &threads,
            self.next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
            MAX_SNAPSHOT_BYTES,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.persistence_dirty = true;
        Ok(())
    }

    /// Imports one legacy overlay as an all-or-nothing local transaction.
    ///
    /// The prospective batch allocates stable reply IDs and validates the
    /// exact serialized snapshot before this store changes. It intentionally
    /// emits no [`ThreadChange`] because migration must never replay prior
    /// local mutations to a transport adapter.
    pub(crate) fn import_legacy(
        &mut self,
        mutations: impl IntoIterator<Item = LegacyThreadMutation>,
    ) -> Result<(), ThreadError> {
        self.import_legacy_with_snapshot_limit(mutations, MAX_SNAPSHOT_BYTES)
    }

    fn import_legacy_with_snapshot_limit(
        &mut self,
        mutations: impl IntoIterator<Item = LegacyThreadMutation>,
        snapshot_limit: usize,
    ) -> Result<(), ThreadError> {
        let mutations = mutations.into_iter().collect::<Vec<_>>();
        if mutations.is_empty() {
            return Ok(());
        }

        let mut threads = self.threads.clone();
        let mut next_local_reply_id = self.next_local_reply_id;
        for mutation in mutations {
            match mutation {
                LegacyThreadMutation::Reply { thread, body } => {
                    if !thread.is_valid() {
                        return Err(ThreadError::InvalidThread);
                    }
                    let body = body.trim();
                    if body.is_empty() {
                        return Err(ThreadError::EmptyReply);
                    }
                    if !valid_reply(body) {
                        return Err(ThreadError::InvalidReply);
                    }
                    if !threads.contains_key(&thread) && threads.len() >= MAX_THREADS {
                        return Err(ThreadError::ThreadCapacity);
                    }
                    if threads
                        .get(&thread)
                        .is_some_and(|state| state.replies.len() >= MAX_REPLIES_PER_THREAD)
                    {
                        return Err(ThreadError::ReplyCapacity);
                    }
                    let id = next_local_reply_id;
                    let Some(next_id) = id.checked_add(1) else {
                        return Err(ThreadError::ReplyIdExhausted);
                    };
                    threads
                        .entry(thread)
                        .or_default()
                        .replies
                        .push(LocalThreadReply {
                            id,
                            body: body.into(),
                        });
                    next_local_reply_id = next_id;
                }
                LegacyThreadMutation::SelectedReactions {
                    thread,
                    target,
                    selected_icons,
                } => {
                    if !thread.is_valid() {
                        return Err(ThreadError::InvalidThread);
                    }
                    if !target.is_well_formed() {
                        return Err(ThreadError::InvalidTarget);
                    }
                    if selected_icons.is_empty()
                        || selected_icons.len() > MAX_SELECTED_REACTIONS_PER_TARGET
                    {
                        return Err(ThreadError::InvalidReaction);
                    }
                    let mut seen_icons = BTreeSet::new();
                    if selected_icons
                        .iter()
                        .any(|icon| !valid_reaction_icon(icon) || !seen_icons.insert(icon.as_str()))
                    {
                        return Err(ThreadError::InvalidReaction);
                    }
                    if let ThreadTarget::LocalReply { reply_id } = &target
                        && !threads.get(&thread).is_some_and(|state| {
                            state.replies.iter().any(|reply| reply.id == *reply_id)
                        })
                    {
                        return Err(ThreadError::MissingLocalReply);
                    }
                    if !threads.contains_key(&thread) && threads.len() >= MAX_THREADS {
                        return Err(ThreadError::ThreadCapacity);
                    }
                    let state = threads.entry(thread).or_default();
                    if !state.selected_reactions.contains_key(&target)
                        && state.selected_reactions.len() >= MAX_REACTION_TARGETS_PER_THREAD
                    {
                        return Err(ThreadError::ReactionTargetCapacity);
                    }
                    // Activity and Teams v1 could both contain selections for
                    // the same shared post. They represent one local user's
                    // choices, so retain their union rather than allowing the
                    // later source import to erase an earlier selection.
                    let merged = state
                        .selected_reactions
                        .get(&target)
                        .into_iter()
                        .flatten()
                        .cloned()
                        .chain(selected_icons)
                        .fold(Vec::new(), |mut icons, icon| {
                            if !icons.contains(&icon) {
                                icons.push(icon);
                            }
                            icons
                        });
                    if merged.len() > MAX_SELECTED_REACTIONS_PER_TARGET {
                        return Err(ThreadError::ReactionCapacity);
                    }
                    state.selected_reactions.insert(target, merged);
                }
            }
        }

        if !Self::snapshot_fits(
            &threads,
            next_local_reply_id,
            // Reserve room for both durable migration markers even when the
            // caller imports one legacy source at a time. That guarantees a
            // successful batch can be followed by its marker without making
            // an otherwise-saveable canonical overlay unsaveable.
            true,
            true,
            snapshot_limit,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.next_local_reply_id = next_local_reply_id;
        self.persistence_dirty = true;
        Ok(())
    }

    /// Toggles the current user's reaction contribution after validating an
    /// addressable target. For local replies, the ID must belong to this exact
    /// thread; source-reply keys remain feature-owned reference identifiers.
    pub(crate) fn toggle_reaction(
        &mut self,
        thread: ThreadId,
        target: ThreadTarget,
        seed: &[ReactionSeed],
        icon: &str,
    ) -> Result<bool, ThreadError> {
        self.toggle_reaction_with_snapshot_limit(thread, target, seed, icon, MAX_SNAPSHOT_BYTES)
    }

    fn toggle_reaction_with_snapshot_limit(
        &mut self,
        thread: ThreadId,
        target: ThreadTarget,
        seed: &[ReactionSeed],
        icon: &str,
        snapshot_limit: usize,
    ) -> Result<bool, ThreadError> {
        if !thread.is_valid() {
            return Err(ThreadError::InvalidThread);
        }
        if !target.is_well_formed() {
            return Err(ThreadError::InvalidTarget);
        }
        if !valid_reaction_icon(icon) {
            return Err(ThreadError::InvalidReaction);
        }
        if let ThreadTarget::LocalReply { reply_id } = &target
            && !self
                .threads
                .get(&thread)
                .is_some_and(|state| state.replies.iter().any(|reply| reply.id == *reply_id))
        {
            return Err(ThreadError::MissingLocalReply);
        }
        if !self.threads.contains_key(&thread) && self.threads.len() >= MAX_THREADS {
            return Err(ThreadError::ThreadCapacity);
        }

        let mut threads = self.threads.clone();
        let state = threads.entry(thread.clone()).or_default();
        let was_selected = state
            .selected_reactions
            .get(&target)
            .is_some_and(|icons| icons.iter().any(|selected| selected == icon));
        if !was_selected
            && state
                .selected_reactions
                .get(&target)
                .is_some_and(|icons| icons.len() >= MAX_SELECTED_REACTIONS_PER_TARGET)
        {
            return Err(ThreadError::ReactionCapacity);
        }
        if !state.selected_reactions.contains_key(&target)
            && state.selected_reactions.len() >= MAX_REACTION_TARGETS_PER_THREAD
        {
            return Err(ThreadError::ReactionTargetCapacity);
        }

        let selected_icons = state
            .selected_reactions
            .get(&target)
            .map_or(&[][..], Vec::as_slice);
        let mut reactions = ReactionSet::from_seed_with_selected(seed, selected_icons);
        let selected = reactions.toggle(icon);
        let selected_icons = reactions.selected_icons();
        if selected_icons.is_empty() {
            state.selected_reactions.remove(&target);
        } else {
            state
                .selected_reactions
                .insert(target.clone(), selected_icons);
        }
        if !Self::snapshot_fits(
            &threads,
            self.next_local_reply_id,
            self.activity_v1_imported,
            self.teams_v1_imported,
            snapshot_limit,
        ) {
            return Err(ThreadError::SnapshotCapacity);
        }
        self.threads = threads;
        self.persistence_dirty = true;
        self.pending_changes.push(ThreadChange::ReactionChanged {
            thread,
            target,
            reaction: icon.into(),
            selected,
        });
        Ok(selected)
    }

    /// Drains transient accepted changes without persisting or replaying them.
    pub(crate) fn take_changes(&mut self) -> Vec<ThreadChange> {
        std::mem::take(&mut self.pending_changes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadError {
    InvalidThread,
    InvalidTarget,
    EmptyReply,
    InvalidReply,
    InvalidReaction,
    ThreadCapacity,
    ReplyCapacity,
    ReactionTargetCapacity,
    ReactionCapacity,
    MissingLocalReply,
    ReplyIdExhausted,
    SnapshotCapacity,
}

impl fmt::Display for ThreadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidThread => "the discussion target is invalid",
            Self::InvalidTarget => "the reaction target is invalid",
            Self::EmptyReply => "a reply needs text",
            Self::InvalidReply => "the reply cannot be saved locally",
            Self::InvalidReaction => "the reaction is invalid",
            Self::ThreadCapacity => "too many local discussions are stored",
            Self::ReplyCapacity => "this discussion has too many local replies",
            Self::ReactionTargetCapacity => "this discussion has too many reaction targets",
            Self::ReactionCapacity => "this reaction target has too many selected reactions",
            Self::MissingLocalReply => "the local reply no longer exists in this discussion",
            Self::ReplyIdExhausted => "local reply identifiers are exhausted",
            Self::SnapshotCapacity => "the local discussion snapshot is full",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ThreadError {}

fn valid_key(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_THREAD_KEY_BYTES
        && !value.chars().any(char::is_control)
}

pub(super) fn valid_reply(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_REPLY_BYTES && !value.contains('\0')
}

pub(super) fn valid_reaction_icon(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_REACTION_ICON_BYTES
        && value.chars().count() <= 16
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{
        LegacyThreadMutation, LocalThreadReply, StoredThreads, ThreadChange, ThreadError, ThreadId,
        ThreadStore, ThreadTarget,
    };
    use crate::ui::reactions::ReactionSeed;

    fn thread(channel: &str, post_key: &str) -> ThreadId {
        ThreadId::new(channel, post_key).expect("fixture thread must be valid")
    }

    #[test]
    fn local_replies_have_stable_monotonic_ids_and_transient_changes() {
        let mut store = ThreadStore::default();
        let id = thread("Bike Commuters · General", "route-sync");

        let first = store
            .append_reply(id.clone(), "  Ship the evaluation notes.  ")
            .expect("reply must be accepted");
        let second = store
            .append_reply(id.clone(), "I will review the metrics.")
            .expect("reply must be accepted");

        assert_eq!(first.id(), 1);
        assert_eq!(first.body(), "Ship the evaluation notes.");
        assert_eq!(second.id(), 2);
        assert_eq!(store.replies(&id), [first.clone(), second.clone()]);
        assert!(store.is_persistence_dirty());
        assert_eq!(
            store.take_changes(),
            vec![
                ThreadChange::ReplyAdded {
                    thread: id.clone(),
                    reply: first,
                },
                ThreadChange::ReplyAdded {
                    thread: id,
                    reply: second,
                },
            ]
        );
        assert!(store.take_changes().is_empty());
    }

    #[test]
    fn reactions_are_scoped_to_exact_thread_and_target() {
        let mut store = ThreadStore::default();
        let primary = thread("Bike Commuters · General", "route-sync");
        let other_channel = thread("Other · General", "route-sync");
        let source_reply = ThreadTarget::source_reply("marco-review").expect("valid source key");
        let seed = [ReactionSeed {
            icon: "👍",
            count: 4,
        }];

        assert!(
            store
                .toggle_reaction(primary.clone(), source_reply.clone(), &seed, "👍")
                .expect("reaction must be accepted")
        );
        assert!(
            store
                .reactions(&primary, &source_reply, &seed)
                .current_user_reacted("👍")
        );
        assert!(
            !store
                .reactions(&other_channel, &source_reply, &seed)
                .current_user_reacted("👍")
        );
        assert!(
            store
                .toggle_reaction(other_channel.clone(), ThreadTarget::Post, &[], "❤")
                .expect("separate target must be accepted")
        );
        assert!(
            store
                .reactions(&other_channel, &ThreadTarget::Post, &[])
                .current_user_reacted("❤")
        );
    }

    #[test]
    fn local_reply_reactions_require_an_existing_reply_in_the_same_thread() {
        let mut store = ThreadStore::default();
        let primary = thread("Bike Commuters · General", "route-sync");
        let other = thread("Bike Commuters · General", "other-post");
        let reply = store
            .append_reply(primary.clone(), "A local reply")
            .expect("reply must be accepted");
        let target = ThreadTarget::local_reply(reply.id()).expect("nonzero reply id");

        assert!(
            store
                .toggle_reaction(primary, target.clone(), &[], "❤")
                .expect("existing local reply must accept reactions")
        );
        assert_eq!(
            store.toggle_reaction(other, target, &[], "❤"),
            Err(ThreadError::MissingLocalReply)
        );
    }

    #[test]
    fn stored_round_trip_keeps_ids_and_selections_but_drops_transient_changes() {
        let mut store = ThreadStore::default();
        let id = thread("Bike Commuters · General", "route-sync");
        let reply = store
            .append_reply(id.clone(), "Keep this across restart")
            .expect("reply must be accepted");
        let target = ThreadTarget::local_reply(reply.id()).expect("nonzero reply id");
        assert!(
            store
                .toggle_reaction(id.clone(), target.clone(), &[], "😄")
                .expect("reaction must be accepted")
        );
        let stored = store.stored();

        let mut restored = ThreadStore::from_stored(stored);
        assert_eq!(
            restored.replies(&id),
            [LocalThreadReply {
                id: 1,
                body: "Keep this across restart".into(),
            }]
        );
        assert!(
            restored
                .reactions(&id, &target, &[])
                .current_user_reacted("😄")
        );
        assert!(restored.take_changes().is_empty());
        assert!(!restored.is_persistence_dirty());
        assert_eq!(
            restored
                .append_reply(id, "Next stable reply")
                .expect("next reply must be accepted")
                .id(),
            2
        );
    }

    #[test]
    fn editing_a_local_reply_keeps_its_id_and_reactions_across_restart() {
        let mut store = ThreadStore::default();
        let id = thread("Bike Commuters · General", "route-sync");
        let reply = store
            .append_reply(id.clone(), "Initial wording")
            .expect("reply must be accepted");
        let target = ThreadTarget::local_reply(reply.id()).expect("reply ID must be nonzero");
        store
            .toggle_reaction(id.clone(), target.clone(), &[], "❤")
            .expect("local reply reaction must be accepted");
        let _ = store.take_changes();

        let updated = store
            .edit_reply(id.clone(), reply.id(), "  Revised wording  ")
            .expect("valid edit must be accepted");

        assert_eq!(updated.id(), reply.id());
        assert_eq!(updated.body(), "Revised wording");
        assert!(store.reactions(&id, &target, &[]).current_user_reacted("❤"));
        assert_eq!(
            store.take_changes(),
            vec![ThreadChange::ReplyEdited {
                thread: id.clone(),
                reply: updated,
            }]
        );

        let restored = ThreadStore::from_stored(store.stored());
        assert_eq!(restored.replies(&id)[0].id(), reply.id());
        assert_eq!(restored.replies(&id)[0].body(), "Revised wording");
        assert!(
            restored
                .reactions(&id, &target, &[])
                .current_user_reacted("❤")
        );
    }

    #[test]
    fn deleting_a_local_reply_clears_only_its_reactions_and_never_reuses_its_id() {
        let mut store = ThreadStore::default();
        let id = thread("Bike Commuters · General", "route-sync");
        let first = store
            .append_reply(id.clone(), "Delete this reply")
            .expect("reply must be accepted");
        let second = store
            .append_reply(id.clone(), "Keep this reply")
            .expect("reply must be accepted");
        let first_target = ThreadTarget::local_reply(first.id()).expect("reply ID must be nonzero");
        store
            .toggle_reaction(id.clone(), first_target.clone(), &[], "👍")
            .expect("local reply reaction must be accepted");
        store
            .toggle_reaction(id.clone(), ThreadTarget::Post, &[], "❤")
            .expect("post reaction must be accepted");
        let _ = store.take_changes();

        store
            .delete_reply(id.clone(), first.id())
            .expect("existing local reply must delete");

        assert_eq!(store.replies(&id), [second]);
        assert!(
            !store
                .reactions(&id, &first_target, &[])
                .current_user_reacted("👍")
        );
        assert!(
            store
                .reactions(&id, &ThreadTarget::Post, &[])
                .current_user_reacted("❤")
        );
        assert_eq!(
            store.take_changes(),
            vec![ThreadChange::ReplyDeleted {
                thread: id.clone(),
                reply_id: first.id(),
            }]
        );
        assert_eq!(
            store
                .append_reply(id, "A new reply receives a fresh ID")
                .expect("reply must be accepted")
                .id(),
            3
        );
    }

    #[test]
    fn rejected_reply_edits_leave_the_snapshot_and_pending_changes_unchanged() {
        let mut store = ThreadStore::default();
        let id = thread("Bike Commuters · General", "route-sync");
        let reply = store
            .append_reply(id.clone(), "Keep this wording")
            .expect("reply must be accepted");
        let _ = store.take_changes();
        let before = store.stored();

        assert_eq!(
            store.edit_reply(id.clone(), reply.id(), "   "),
            Err(ThreadError::EmptyReply)
        );
        assert_eq!(
            store.edit_reply_with_snapshot_limit(id.clone(), reply.id(), "Too large", 1),
            Err(ThreadError::SnapshotCapacity)
        );
        assert_eq!(store.stored(), before);
        assert!(store.take_changes().is_empty());
    }

    #[test]
    fn invalid_identifiers_and_replies_are_rejected_before_mutation() {
        assert!(ThreadId::new("", "post").is_none());
        assert!(ThreadId::new("channel", "post\0").is_none());
        assert!(ThreadTarget::source_reply("\n").is_none());
        assert!(ThreadTarget::local_reply(0).is_none());

        let mut store = ThreadStore::default();
        let id = thread("channel", "post");
        assert_eq!(
            store.append_reply(id.clone(), "   "),
            Err(ThreadError::EmptyReply)
        );
        assert_eq!(
            store.append_reply(id, "reply\0body"),
            Err(ThreadError::InvalidReply)
        );
        assert!(!store.is_persistence_dirty());
        assert_eq!(store.stored(), StoredThreads::default());
    }

    #[test]
    fn snapshot_preflight_rejects_a_reply_without_consuming_an_id_or_change() {
        let mut store = ThreadStore::default();
        let id = thread("channel", "post");
        let before = store.stored();

        assert_eq!(
            store.append_reply_with_snapshot_limit(id.clone(), "Will not fit", 1),
            Err(ThreadError::SnapshotCapacity)
        );
        assert_eq!(store.stored(), before);
        assert!(!store.is_persistence_dirty());
        assert!(store.take_changes().is_empty());
        assert_eq!(
            store
                .append_reply(id, "The first accepted reply still uses id one")
                .expect("normal snapshot must fit")
                .id(),
            1
        );
    }

    #[test]
    fn snapshot_preflight_rejects_a_reaction_without_mutating_the_overlay() {
        let mut store = ThreadStore::default();
        let id = thread("channel", "post");

        assert_eq!(
            store
                .toggle_reaction_with_snapshot_limit(id.clone(), ThreadTarget::Post, &[], "👍", 1,),
            Err(ThreadError::SnapshotCapacity)
        );
        assert!(
            !store
                .reactions(&id, &ThreadTarget::Post, &[])
                .current_user_reacted("👍")
        );
        assert!(!store.is_persistence_dirty());
        assert!(store.take_changes().is_empty());
    }

    #[test]
    fn migration_markers_are_durable_and_only_make_the_store_dirty_once() {
        let mut store = ThreadStore::default();
        assert!(!store.activity_v1_imported());
        assert!(!store.teams_v1_imported());

        store
            .mark_activity_v1_imported()
            .expect("small migration marker must fit");
        store
            .mark_teams_v1_imported()
            .expect("small migration marker must fit");
        assert!(store.is_persistence_dirty());
        store.mark_persisted();
        store
            .mark_activity_v1_imported()
            .expect("existing marker must remain valid");
        store
            .mark_teams_v1_imported()
            .expect("existing marker must remain valid");
        assert!(!store.is_persistence_dirty());

        let restored = ThreadStore::from_stored(store.stored());
        assert!(restored.activity_v1_imported());
        assert!(restored.teams_v1_imported());
        assert!(!restored.is_persistence_dirty());
    }

    #[test]
    fn legacy_import_commits_replies_and_selections_without_transport_changes() {
        let mut store = ThreadStore::default();
        let id = thread("Juniper Row All Hands · Facilities", "parking-changes");
        let target = ThreadTarget::source_reply("gate-question").expect("fixture target");

        store
            .import_legacy([
                LegacyThreadMutation::Reply {
                    thread: id.clone(),
                    body: "  Keep the coverage note.  ".into(),
                },
                LegacyThreadMutation::SelectedReactions {
                    thread: id.clone(),
                    target: target.clone(),
                    selected_icons: vec!["❤".into()],
                },
            ])
            .expect("legacy batch must import");

        assert_eq!(store.replies(&id)[0].id(), 1);
        assert_eq!(store.replies(&id)[0].body(), "Keep the coverage note.");
        assert!(store.reactions(&id, &target, &[]).current_user_reacted("❤"));
        assert!(store.take_changes().is_empty());
        assert!(store.is_persistence_dirty());
    }

    #[test]
    fn legacy_import_unions_selection_sets_for_the_same_shared_target() {
        let mut store = ThreadStore::default();
        let id = thread("Juniper Row All Hands · Facilities", "parking-changes");
        let target = ThreadTarget::source_reply("gate-question").expect("fixture target");

        store
            .import_legacy([
                LegacyThreadMutation::SelectedReactions {
                    thread: id.clone(),
                    target: target.clone(),
                    selected_icons: vec!["❤".into()],
                },
                LegacyThreadMutation::SelectedReactions {
                    thread: id.clone(),
                    target: target.clone(),
                    selected_icons: vec!["👍".into(), "❤".into()],
                },
            ])
            .expect("both legacy selection sets must merge");

        assert_eq!(
            store.reactions(&id, &target, &[]).selected_icons(),
            vec!["❤", "👍"]
        );
        assert!(store.take_changes().is_empty());
    }

    #[test]
    fn legacy_import_is_all_or_nothing_for_late_validation_or_capacity_failure() {
        let mut store = ThreadStore::default();
        let id = thread("Juniper Row All Hands · Facilities", "parking-changes");
        let before = store.stored();

        assert_eq!(
            store.import_legacy([
                LegacyThreadMutation::Reply {
                    thread: id.clone(),
                    body: "Would be first".into(),
                },
                LegacyThreadMutation::SelectedReactions {
                    thread: id.clone(),
                    target: ThreadTarget::Post,
                    selected_icons: vec!["\0".into()],
                },
            ]),
            Err(ThreadError::InvalidReaction)
        );
        assert_eq!(store.stored(), before);
        assert!(!store.is_persistence_dirty());
        assert!(store.take_changes().is_empty());

        assert_eq!(
            store.import_legacy_with_snapshot_limit(
                [LegacyThreadMutation::Reply {
                    thread: id,
                    body: "Would overflow the snapshot".into(),
                }],
                1,
            ),
            Err(ThreadError::SnapshotCapacity)
        );
        assert_eq!(store.stored(), before);
        assert!(!store.is_persistence_dirty());
        assert!(store.take_changes().is_empty());
    }
}
