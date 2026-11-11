use super::WorkspaceTransportController;
use super::remote_worker::{
    InterestKind, RemotePoll, RemotePollPriority, RemoteWork, TransportRuntime,
};
use crate::integrations::{
    CommandTargetProvider, ProviderResourceRef, RemoteChatMembersProjection,
    RemoteChatMessagesProjection, RemoteMessageSearchProjection,
    RemotePinnedChatMessagesProjection, RemoteSearchQuery, SyncInterest, TransportError,
    WorkspaceDelta,
};
use std::time::{Duration, Instant};

/// Graph may omit `Retry-After` on a throttled read. Keep the next read of
/// that kind out of a tight retry loop in that case.
pub(super) const POLL_THROTTLE_FALLBACK: Duration = Duration::from_secs(2);

/// A session-only embargo after Graph throttles a read of one interest kind.
/// Representing the duration relative to the observed instant avoids turning
/// an unusually large provider `Retry-After` value into an overflow that
/// would accidentally permit an immediate retry.
#[derive(Clone, Copy)]
pub(super) struct PollCooldown {
    pub(super) started_at: Instant,
    pub(super) duration: Duration,
}

impl PollCooldown {
    pub(super) fn blocks_at(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < self.duration
    }

    pub(super) fn remaining_at(self, now: Instant) -> Duration {
        self.duration
            .saturating_sub(now.saturating_duration_since(self.started_at))
    }
}

impl WorkspaceTransportController {
    /// Preserves app-owned poll provenance until completion. Normal callers
    /// use [`Self::handle_completed_poll`]; only a quote reconciliation poll
    /// is allowed to complete a target-bound ambiguous quote reply.
    pub(super) fn handle_completed_remote_poll(
        &mut self,
        poll: RemotePoll,
        result: Result<WorkspaceDelta, TransportError>,
    ) -> Result<(), TransportError> {
        let quote_reconciliation_chat = poll.message_reconciliation_chat();
        let interest = poll.into_interest();
        let completed = self.handle_completed_poll(interest.clone(), result);
        if let Some(chat) = quote_reconciliation_chat {
            match &completed {
                Ok(()) => {
                    let received_expected_history =
                        self.pending_deltas.back().is_some_and(|delta| {
                            delta.completed.contains(&interest)
                                && delta.remote_chat_messages.is_some()
                        });
                    if received_expected_history {
                        self.complete_pending_message_reconciliations(chat);
                    } else {
                        self.fail_pending_message_reconciliations(
                            chat,
                            TransportError::Unavailable(
                                "Microsoft Graph did not return selected chat history for quote reply reconciliation"
                                    .into(),
                            ),
                        );
                    }
                }
                Err(error) => {
                    self.fail_pending_message_reconciliations(chat, error.clone());
                }
            }
        }
        completed
    }

    pub(super) fn start_next_remote_poll(&mut self) -> Result<(), TransportError> {
        self.start_next_remote_poll_at(Instant::now())
    }

    /// Starts the best queued poll that is allowed to run now: the lowest
    /// priority tier first (FIFO within a tier), skipping any kind that is
    /// inside a Retry-After cooldown. Directory/bootstrap reads additionally
    /// yield to a due Microsoft 365 command so a send is never queued behind
    /// up to ~70 bootstrap GETs. Nothing is dropped: skipped polls stay
    /// queued and run once the command or cooldown clears.
    pub(super) fn start_next_remote_poll_at(&mut self, now: Instant) -> Result<(), TransportError> {
        let command_waiting = self.remote_command_due_at(now);
        let next = self
            .pending_remote_interests
            .iter()
            .enumerate()
            .filter(|(_, poll)| {
                !self.poll_cooldown_blocks_at(poll.kind(), now)
                    && !(command_waiting && poll.priority() == RemotePollPriority::Directory)
            })
            .min_by_key(|(index, poll)| (poll.priority(), *index))
            .map(|(index, _)| index);
        let Some(poll) = next.and_then(|index| self.pending_remote_interests.remove(index)) else {
            return Ok(());
        };
        self.start_remote_poll(poll)
    }

    /// Whether the FIFO head is a Microsoft 365 command ready to dispatch.
    pub(super) fn remote_command_due_at(&self, now: Instant) -> bool {
        self.pending_commands.front().is_some_and(|queued| {
            queued.next_attempt_at <= now
                && matches!(
                    queued.command.target_provider(),
                    Ok(CommandTargetProvider::Microsoft365)
                )
        })
    }

    pub(super) fn poll_cooldown_blocks_at(&self, kind: InterestKind, now: Instant) -> bool {
        self.poll_cooldowns
            .get(&kind)
            .is_some_and(|cooldown| cooldown.blocks_at(now))
    }

    /// The shortest remaining cooldown among queued polls, so the app can
    /// schedule a repaint for the moment a deferred read may start.
    pub(super) fn next_poll_cooldown_delay_at(&self, now: Instant) -> Option<Duration> {
        self.pending_remote_interests
            .iter()
            .filter_map(|poll| {
                let cooldown = self.poll_cooldowns.get(&poll.kind())?;
                cooldown.blocks_at(now).then(|| cooldown.remaining_at(now))
            })
            .min()
    }

    /// Cancels a running directory poll so a due user command can start
    /// within one Graph request instead of after the whole bootstrap fan-out.
    /// Each poll is preempted at most once; `collect_remote_work` re-queues
    /// it at the front when the worker returns.
    pub(super) fn preempt_directory_poll_for_command(&mut self) {
        if self.preempting_directory_poll {
            return;
        }
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            generation,
            ..
        } = &self.runtime
            && active.priority() == RemotePollPriority::Directory
            && !active.was_preempted()
            && !cancellation.is_cancelled()
            && *generation == self.remote_generation
        {
            cancellation.cancel();
            self.preempting_directory_poll = true;
        }
    }

    pub(super) fn handle_completed_poll(
        &mut self,
        interest: SyncInterest,
        result: Result<WorkspaceDelta, TransportError>,
    ) -> Result<(), TransportError> {
        self.handle_completed_poll_at(interest, result, Instant::now())
    }

    /// The time-injected form keeps server-directed poll backoff testable
    /// without making production callers responsible for clock handling.
    pub(super) fn handle_completed_poll_at(
        &mut self,
        interest: SyncInterest,
        result: Result<WorkspaceDelta, TransportError>,
        now: Instant,
    ) -> Result<(), TransportError> {
        let delta = match result {
            Err(TransportError::Cancelled) => return Ok(()),
            Err(error) => {
                if interest != SyncInterest::Identity
                    && let TransportError::Throttled {
                        retry_after_seconds,
                    } = &error
                {
                    self.apply_poll_cooldown(
                        InterestKind::from(&interest),
                        *retry_after_seconds,
                        now,
                    );
                }
                // A selected read that fails transiently must not leave its
                // Loading state stuck. Hand the feature a target-bound,
                // row-free marker so it can return to a retryable state; the
                // error itself still surfaces unchanged.
                if !matches!(error, TransportError::ReauthenticationRequired)
                    && let Some(delta) = unavailable_selected_chat_delta(&interest)
                {
                    self.pending_deltas.push_back(delta);
                }
                return Err(error);
            }
            Ok(delta) => delta,
        };
        // Remote projections are meaningful only after a fresh authenticated
        // identity. Queue each bounded, read-only surface exactly once for
        // this bootstrap sequence. The serial worker keeps their Graph fanout
        // ordered and ensures a new profile can invalidate every result.
        if self.is_microsoft365_active()
            && delta.completed.contains(&SyncInterest::Identity)
            && delta.identity.is_some()
        {
            self.queue_remote_interest(SyncInterest::Chats);
            self.queue_remote_interest(SyncInterest::Teams);
            self.queue_remote_interest(SyncInterest::Calendar);
            self.queue_remote_interest(SyncInterest::Files);
        }
        self.pending_notice = delta.notices.first().cloned();
        self.pending_deltas.push_back(delta);
        Ok(())
    }

    pub(super) fn queue_remote_chat_refresh(&mut self) {
        self.queue_remote_interest(SyncInterest::Chats);
    }

    /// Whether a read of this kind is active, queued, or inside a
    /// Retry-After cooldown, so one more request would only add load.
    pub(super) fn remote_kind_busy_at(&self, kind: InterestKind, now: Instant) -> bool {
        let active = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.kind() == kind
        );
        active
            || self
                .pending_remote_interests
                .iter()
                .any(|pending| pending.kind() == kind)
            || self.poll_cooldown_blocks_at(kind, now)
    }

    /// Queues one account-wide chat directory refresh for an explicit user
    /// event. It is a no-op while a directory read is already active, queued
    /// or cooled down, so repeated events never build a fan-out. It never
    /// touches a delivery or reconciliation poll.
    pub(crate) fn request_remote_chats_refresh(&mut self) -> bool {
        if !self.is_microsoft365_active()
            || self.remote_kind_busy_at(InterestKind::Chats, Instant::now())
        {
            return false;
        }
        self.queue_remote_chat_refresh();
        true
    }

    /// Queues one exact selected-chat history refresh after a chat-message
    /// reaction outcome. It deliberately bypasses
    /// `request_remote_chat_messages`: a delivery reconciliation must retain
    /// the original opaque chat target and must not replace or cancel a
    /// user's current selected-chat history read.
    pub(super) fn queue_remote_chat_messages_refresh(&mut self, chat: ProviderResourceRef) {
        self.queue_remote_poll(RemotePoll::delivery_chat_messages_refresh(
            SyncInterest::ChatMessages { chat },
        ));
    }

    /// Requests the bounded history for one selected remote chat. This is a
    /// selection-scoped read, distinct from the account-wide Chats projection
    /// used for directory refresh and chat-send reconciliation.
    ///
    /// At most one selected-chat history poll may be active or queued.
    /// Replacing it cancels only an obsolete ChatMessages read; global Chats,
    /// Teams/channel projections, and command delivery remain untouched.
    pub(crate) fn request_remote_chat_messages(&mut self, chat: ProviderResourceRef) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChatMessages { chat };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_chat_messages()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other_chat_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_chat_messages() && !active_same_selection
        );
        if active_other_chat_selection {
            self.cancel_remote_chat_messages();
        }

        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_chat_messages() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_chat_messages() || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_chat_messages(requested));
        }
    }

    /// Drops queued selected-chat history reads and asks an active one to stop
    /// cooperatively. The global Chats projection and all other transport work
    /// deliberately retain their independent lifecycles.
    pub(crate) fn cancel_remote_chat_messages(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_chat_messages());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_chat_messages()
        {
            cancellation.cancel();
        }
    }

    /// Requests the bounded pinned-message projection for one selected remote
    /// chat. It is deliberately independent from the main `ChatMessages`
    /// history: opening or closing the contextual pins panel must never
    /// replace the conversation feed, a delivery reconciliation, or another
    /// selected read.
    ///
    /// At most one selected pinned-message poll may be active or queued. A
    /// repeated request for the same opaque chat is deduplicated; a changed
    /// selection cancels only the obsolete pins read before queuing its
    /// replacement.
    pub(crate) fn request_remote_chat_pinned_messages(&mut self, chat: ProviderResourceRef) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChatPinnedMessages { chat };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_chat_pinned_messages()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_chat_pinned_messages() && !active_same_selection
        );
        if active_other_selection {
            self.cancel_remote_chat_pinned_messages();
        }

        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_chat_pinned_messages() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_chat_pinned_messages() || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_chat_pinned_messages(requested));
        }
    }

    /// Drops queued selected-chat pinned-message reads and asks an active one
    /// to stop cooperatively. The global directory, normal selected history,
    /// delivery reconciliation, and every other selected read keep their own
    /// lifecycles.
    pub(crate) fn cancel_remote_chat_pinned_messages(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_chat_pinned_messages());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_chat_pinned_messages()
        {
            cancellation.cancel();
        }
    }

    /// Requests the bounded People roster for one selected remote chat. It is
    /// intentionally independent from history and pinned messages: opening or
    /// closing the contextual roster must not replace a conversation feed,
    /// delivery reconciliation, or another selected-chat surface.
    ///
    /// At most one selected member-roster poll may be active or queued. A
    /// repeated request for the same opaque chat is deduplicated only while
    /// the active worker can still produce a result; a cancellation-pending
    /// worker is replaceable so an immediate panel reopen cannot stay loading.
    pub(crate) fn request_remote_chat_members(&mut self, chat: ProviderResourceRef) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChatMembers { chat };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_chat_members()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_chat_members() && !active_same_selection
        );
        if active_other_selection {
            self.cancel_remote_chat_members();
        }

        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_chat_members() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_chat_members() || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_chat_members(requested));
        }
    }

    /// Drops queued selected-chat member-roster reads and asks an active one
    /// to stop cooperatively. Chat history, pins, global directory work, and
    /// every other selected read deliberately retain their own lifecycles.
    pub(crate) fn cancel_remote_chat_members(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_chat_members());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_chat_members()
        {
            cancellation.cancel();
        }
    }

    /// Requests one page of the user's explicit Microsoft 365 message search.
    ///
    /// At most one search poll is active or queued. A new query or page
    /// replaces an obsolete one: the queued copy is dropped and an active
    /// different search is asked to stop. An identical request is
    /// deduplicated only while its worker can still produce a result. It has
    /// its own cooldown kind and never touches chat history or any other read.
    pub(crate) fn request_remote_message_search(&mut self, query: RemoteSearchQuery, page: u8) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::MessageSearch { query, page };
        let active_same = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_message_search()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_message_search() && !active_same
        );
        if active_other {
            self.cancel_remote_message_search();
        }
        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_message_search() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_message_search() || pending.as_interest() == &requested
        });
        if !active_same && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_message_search(requested));
        }
    }

    /// Drops a queued message search and asks an active one to stop. Chat
    /// history, pins, members, and global directory work are unaffected.
    pub(crate) fn cancel_remote_message_search(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_message_search());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_message_search()
        {
            cancellation.cancel();
        }
    }

    /// Requests one presence-only refresh for the open People roster. At most
    /// one is active or queued; a different chat replaces it. It has its own
    /// cooldown kind and cancellation, and never touches the roster read.
    pub(crate) fn request_remote_chat_member_presence(&mut self, chat: ProviderResourceRef) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChatMemberPresence { chat };
        let active_same = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_chat_member_presence()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        if !active_same {
            self.cancel_remote_chat_member_presence();
        }
        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_chat_member_presence() && pending.as_interest() == &requested
        });
        if !active_same && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_chat_member_presence(requested));
        }
    }

    /// Drops queued presence refreshes and cancels an active one only.
    pub(crate) fn cancel_remote_chat_member_presence(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_chat_member_presence());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_chat_member_presence()
        {
            cancellation.cancel();
        }
    }

    /// Requests one bounded, in-memory preview for the currently selected
    /// OneDrive-root file. This selection-scoped read is distinct from the
    /// root `Files` metadata projection: it must be replaceable without
    /// cancelling the directory, chat/channel reads, or command delivery.
    ///
    /// At most one selected file-preview poll may be active or queued. A
    /// repeated request for the exact opaque item is deduplicated; a new item
    /// cancels only the obsolete selected-preview poll before queuing its
    /// replacement.
    pub(crate) fn request_remote_file_preview(&mut self, item: ProviderResourceRef) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::OneDriveFilePreview { item };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_onedrive_file_preview()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other_file_preview = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_onedrive_file_preview()
                && !active_same_selection
        );
        if active_other_file_preview {
            self.cancel_remote_file_preview();
        }

        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_onedrive_file_preview() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_onedrive_file_preview() || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_onedrive_file_preview(requested));
        }
    }

    /// Drops queued selected OneDrive file previews and asks an active one to
    /// stop cooperatively. Root Files metadata, chat/channel selected reads,
    /// and command delivery deliberately retain their own lifecycles.
    pub(crate) fn cancel_remote_file_preview(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_onedrive_file_preview());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_onedrive_file_preview()
        {
            cancellation.cancel();
        }
    }

    /// Queues one exact channel root-post refresh after a send outcome. It is
    /// intentionally distinct from the user-selection helper below: a
    /// receipt/ambiguous-send refresh must retain its original opaque target
    /// and must not cancel a currently selected channel's independent read.
    pub(super) fn queue_remote_channel_messages_refresh(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) {
        self.queue_remote_interest(SyncInterest::ChannelMessages { team, channel });
    }

    /// Queues one exact root-post reply refresh after that root received a
    /// reply outcome. This bypasses `request_remote_channel_replies`: a
    /// mutation reconciliation must retain its own opaque team/channel/post
    /// target and must not cancel, replace, or otherwise disturb a user's
    /// selected-root read.
    pub(super) fn queue_remote_channel_replies_refresh(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) {
        self.queue_remote_poll(RemotePoll::delivery_channel_replies_refresh(
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            },
        ));
    }

    /// Requests the bounded root-post feed for the currently selected remote
    /// channel. A channel selection is a read, not a user-intent command:
    /// keeping it in the poll queue preserves the controller's one-worker
    /// ordering and lets the adapter return a normalized projection.
    ///
    /// Only the newest selection may remain queued. If a previous selected
    /// channel is already being read, its cooperative cancellation signal is
    /// set before the replacement is queued; no global generation changes, so
    /// account-wide reads and command delivery remain unaffected.
    pub(crate) fn request_remote_channel_messages(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChannelMessages { team, channel };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.as_interest() == &requested && !cancellation.is_cancelled()
        );
        let active_other_channel_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if matches!(active.as_interest(), SyncInterest::ChannelMessages { .. })
                && !active_same_selection
        );
        if active_other_channel_selection {
            self.cancel_active_remote_channel_messages_poll();
        }

        let already_queued = self
            .pending_remote_interests
            .iter()
            .any(|pending| pending.as_interest() == &requested);
        self.pending_remote_interests.retain(|pending| {
            !matches!(pending.as_interest(), SyncInterest::ChannelMessages { .. })
                || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_interest(requested);
        }
    }

    fn cancel_active_remote_channel_messages_poll(&mut self) {
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && matches!(active.as_interest(), SyncInterest::ChannelMessages { .. })
        {
            cancellation.cancel();
        }
    }

    /// Requests the bounded direct-child metadata view for one selected
    /// remote channel's backing files folder. It is a read-only projection,
    /// not a file command: the adapter alone resolves its SharePoint drive
    /// and folder identifiers.
    ///
    /// At most one channel-files poll may be active or queued. Replacing it
    /// cancels only obsolete `ChannelFiles` work; root-post reads, replies,
    /// account bootstrap, and command delivery remain untouched.
    pub(crate) fn request_remote_channel_files(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
    ) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChannelFiles { team, channel };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.as_interest() == &requested && !cancellation.is_cancelled()
        );
        let active_other_channel_files = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if matches!(active.as_interest(), SyncInterest::ChannelFiles { .. })
                && !active_same_selection
        );
        if active_other_channel_files {
            self.cancel_remote_channel_files();
        }

        let already_queued = self
            .pending_remote_interests
            .iter()
            .any(|pending| pending.as_interest() == &requested);
        self.pending_remote_interests.retain(|pending| {
            !matches!(pending.as_interest(), SyncInterest::ChannelFiles { .. })
                || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_interest(requested);
        }
    }

    /// Starts a profile/session-only backoff for one interest kind after
    /// Graph responds with 429. Queued and later reads of that kind are
    /// deferred (not dropped) until it expires.
    fn apply_poll_cooldown(
        &mut self,
        kind: InterestKind,
        retry_after_seconds: Option<u64>,
        now: Instant,
    ) {
        self.poll_cooldowns.insert(
            kind,
            PollCooldown {
                started_at: now,
                duration: poll_throttle_retry_delay(retry_after_seconds),
            },
        );
    }

    /// Drops queued selected-channel file reads and asks an active one to
    /// stop cooperatively. This deliberately leaves all other projections and
    /// command delivery alone.
    pub(crate) fn cancel_remote_channel_files(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !matches!(pending.as_interest(), SyncInterest::ChannelFiles { .. }));
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && matches!(active.as_interest(), SyncInterest::ChannelFiles { .. })
        {
            cancellation.cancel();
        }
    }

    /// Requests the bounded reply feed for one selected remote channel root
    /// post. This is deliberately a separate, selection-scoped read from the
    /// channel root-post feed and never becomes a provider mutation command.
    ///
    /// At most one reply poll may be active or queued. Replacing it cancels
    /// only an obsolete reply read: channel root-post reads, bootstrap
    /// projections, and mutation delivery remain untouched.
    pub(crate) fn request_remote_channel_replies(
        &mut self,
        team: ProviderResourceRef,
        channel: ProviderResourceRef,
        post: ProviderResourceRef,
    ) {
        if !self.is_microsoft365_active() {
            return;
        }
        let requested = SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        };
        let active_same_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                cancellation,
                ..
            } if active.is_selected_channel_replies()
                && active.as_interest() == &requested
                && !cancellation.is_cancelled()
        );
        let active_other_reply_selection = matches!(
            &self.runtime,
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Poll(active),
                ..
            } if active.is_selected_channel_replies() && !active_same_selection
        );
        if active_other_reply_selection {
            self.cancel_remote_channel_replies();
        }

        let already_queued = self.pending_remote_interests.iter().any(|pending| {
            pending.is_selected_channel_replies() && pending.as_interest() == &requested
        });
        self.pending_remote_interests.retain(|pending| {
            !pending.is_selected_channel_replies() || pending.as_interest() == &requested
        });
        if !active_same_selection && !already_queued {
            self.queue_remote_poll(RemotePoll::selected_channel_replies(requested));
        }
    }

    /// Drops queued selected-post reply reads and asks an active reply poll
    /// to stop cooperatively. It intentionally does not affect the selected
    /// channel's root-post poll, other bounded projections, or any command
    /// dispatch.
    pub(crate) fn cancel_remote_channel_replies(&mut self) {
        self.pending_remote_interests
            .retain(|pending| !pending.is_selected_channel_replies());
        if let TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            cancellation,
            ..
        } = &self.runtime
            && active.is_selected_channel_replies()
        {
            cancellation.cancel();
        }
    }

    /// Enqueues one remote projection interest without allowing repeated UI
    /// frames or repeated mutation receipts to build an unbounded fan-out.
    /// The caller remains responsible for choosing only normalized, bounded
    /// interests that the active provider supports.
    pub(super) fn queue_remote_interest(&mut self, interest: SyncInterest) {
        self.queue_remote_poll(RemotePoll::interest(interest));
    }

    /// Queues one app-owned poll request without erasing why it was created.
    /// That provenance matters only to lifecycle cancellation; the provider
    /// receives the wrapped normalized interest unchanged.
    pub(super) fn queue_remote_poll(&mut self, poll: RemotePoll) {
        if self.is_microsoft365_active()
            && !self
                .pending_remote_interests
                .iter()
                .any(|pending| pending == &poll)
        {
            self.pending_remote_interests.push_back(poll);
        }
    }
}

/// Builds the row-free "read failed" delta for a selected chat read
/// (history, pinned messages, or members). Other interests have their own
/// failure semantics and return `None`.
fn unavailable_selected_chat_delta(interest: &SyncInterest) -> Option<WorkspaceDelta> {
    let mut delta = WorkspaceDelta::default();
    match interest {
        SyncInterest::ChatMessages { chat } => {
            delta.remote_chat_messages =
                Some(RemoteChatMessagesProjection::unavailable(chat.clone()).ok()?);
        }
        SyncInterest::ChatPinnedMessages { chat } => {
            delta.remote_pinned_chat_messages =
                Some(RemotePinnedChatMessagesProjection::unavailable(chat.clone()).ok()?);
        }
        SyncInterest::ChatMembers { chat } => {
            delta.remote_chat_members =
                Some(RemoteChatMembersProjection::unavailable(chat.clone()).ok()?);
        }
        SyncInterest::MessageSearch { query, page } => {
            delta.remote_message_search = Some(RemoteMessageSearchProjection::unavailable(
                query.clone(),
                *page,
            ));
        }
        SyncInterest::ChatMemberPresence { chat } => {
            // Keep the labels shown; the feature only reschedules.
            delta.remote_chat_members =
                Some(RemoteChatMembersProjection::presence_failed(chat.clone()).ok()?);
        }
        _ => return None,
    }
    Some(delta)
}

/// A throttled read is never replayed automatically. This delay only defers
/// later reads of the same kind after Graph has returned 429.
pub(super) fn poll_throttle_retry_delay(retry_after_seconds: Option<u64>) -> Duration {
    retry_after_seconds
        .map(Duration::from_secs)
        .unwrap_or(POLL_THROTTLE_FALLBACK)
}
