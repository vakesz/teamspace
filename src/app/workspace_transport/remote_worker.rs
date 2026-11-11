use super::WorkspaceTransportController;
use super::queue::CommandDispatch;
#[cfg(test)]
use crate::integrations::CommandTargetProvider;
use crate::integrations::{
    CommandReceipt, ConnectionStatus, ProviderResourceRef, SyncInterest, TransportCancellation,
    TransportError, WorkspaceCommand, WorkspaceDelta, WorkspaceIntentId, WorkspaceTransport,
};
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(super) enum TransportRuntime {
    /// The built-in adapter is deterministic and has no I/O, so its poll and
    /// dispatch methods may run in the UI update path.
    Local(Box<dyn WorkspaceTransport>),
    /// A remote adapter is idle and available to be moved to a worker.
    RemoteReady(Box<dyn WorkspaceTransport>),
    /// A worker owns the remote adapter until it returns both its result and
    /// the adapter, preserving connection state without shared mutable state.
    /// One serial state covers both reads and mutations, so a network-capable
    /// dispatch never runs on the egui thread or overtakes the FIFO head.
    RemoteBusy {
        receiver: Receiver<RemoteCompletion>,
        cancellation: TransportCancellation,
        generation: u64,
        work: RemoteWork,
    },
    /// A short-lived placeholder used while moving the ready adapter into a
    /// worker. It is never observable after a controller method returns.
    Empty,
}

/// An app-owned polling request keeps why a bounded read was scheduled out of
/// the provider seam. The adapter receives only the normalized
/// `SyncInterest`, while selection replacement can distinguish its own reads
/// from a target-bound post-delivery reconciliation of the same feed.
#[derive(Clone, Debug)]
pub(super) struct RemotePoll {
    interest: SyncInterest,
    purpose: RemotePollPurpose,
    /// Set once a running directory poll was cancelled to let a user command
    /// through and then re-queued. A poll is preempted at most once so a
    /// stream of sends cannot starve bootstrap forever. It is not part of
    /// the poll's identity: a re-queued poll still deduplicates against a
    /// fresh request for the same interest.
    preempted: bool,
}

impl PartialEq for RemotePoll {
    fn eq(&self, other: &Self) -> bool {
        self.interest == other.interest && self.purpose == other.purpose
    }
}

impl Eq for RemotePoll {}

/// Scheduling tier of a queued poll. The single worker always starts the
/// lowest tier first so interactive work is never stuck behind directory
/// bootstrap reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RemotePollPriority {
    /// Identity gates every other projection.
    Identity,
    /// A selection-scoped read or a delivery/reconciliation read that a user
    /// action is waiting for.
    Selected,
    /// Account-wide directory and bootstrap projections.
    Directory,
}

/// The kind of a [`SyncInterest`] without its target. Provider throttling is
/// applied per service, so a Retry-After cooldown is keyed by kind rather
/// than by one selected chat or channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum InterestKind {
    Identity,
    Chats,
    ChatMessages,
    ChatPinnedMessages,
    ChatMembers,
    ChatMemberPresence,
    MessageSearch,
    Teams,
    ChannelMessages,
    ChannelFiles,
    OneDriveFilePreview,
    ChannelReplies,
    Calendar,
    Files,
    All,
}

impl From<&SyncInterest> for InterestKind {
    fn from(interest: &SyncInterest) -> Self {
        match interest {
            SyncInterest::Identity => Self::Identity,
            SyncInterest::Chats => Self::Chats,
            SyncInterest::ChatMessages { .. } => Self::ChatMessages,
            SyncInterest::ChatPinnedMessages { .. } => Self::ChatPinnedMessages,
            SyncInterest::ChatMembers { .. } => Self::ChatMembers,
            SyncInterest::ChatMemberPresence { .. } => Self::ChatMemberPresence,
            SyncInterest::MessageSearch { .. } => Self::MessageSearch,
            SyncInterest::Teams => Self::Teams,
            SyncInterest::ChannelMessages { .. } => Self::ChannelMessages,
            SyncInterest::ChannelFiles { .. } => Self::ChannelFiles,
            SyncInterest::OneDriveFilePreview { .. } => Self::OneDriveFilePreview,
            SyncInterest::ChannelReplies { .. } => Self::ChannelReplies,
            SyncInterest::Calendar => Self::Calendar,
            SyncInterest::Files => Self::Files,
            SyncInterest::All => Self::All,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemotePollPurpose {
    Interest,
    SelectedChatMessages,
    SelectedChatPinnedMessages,
    SelectedChatMembers,
    SelectedChatMemberPresence,
    SelectedMessageSearch,
    SelectedOneDriveFilePreview,
    DeliveryChatMessagesRefresh,
    MessageReconciliation,
    SelectedChannelReplies,
    DeliveryChannelRepliesRefresh,
}

impl RemotePoll {
    pub(super) fn interest(interest: SyncInterest) -> Self {
        Self {
            interest,
            purpose: RemotePollPurpose::Interest,
            preempted: false,
        }
    }

    pub(super) fn selected_channel_replies(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChannelReplies { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedChannelReplies,
            preempted: false,
        }
    }

    pub(super) fn selected_chat_messages(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatMessages { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedChatMessages,
            preempted: false,
        }
    }

    /// Pinned messages are a separate selected-chat read. Keeping their
    /// provenance distinct from the main history means closing the contextual
    /// panel cannot cancel the current conversation feed or a delivery
    /// reconciliation that happens to use the same opaque chat target.
    pub(super) fn selected_chat_pinned_messages(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatPinnedMessages { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedChatPinnedMessages,
            preempted: false,
        }
    }

    /// A People roster is a separate selected-chat read. Its provenance keeps
    /// a contextual member view from cancelling ordinary history, pins, or a
    /// delivery reconciliation for the same opaque chat target.
    pub(super) fn selected_chat_members(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatMembers { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedChatMembers,
            preempted: false,
        }
    }

    /// A People presence refresh is cancellable on its own, so closing the
    /// panel stops only it and a presence cooldown never blocks the roster.
    pub(super) fn selected_chat_member_presence(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatMemberPresence { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedChatMemberPresence,
            preempted: false,
        }
    }

    /// A message search is an explicit user request with its own cooldown
    /// kind and cancellation; it never shares a lifecycle with chat history.
    pub(super) fn selected_message_search(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::MessageSearch { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedMessageSearch,
            preempted: false,
        }
    }

    /// A selected OneDrive file preview is independently cancellable from
    /// the root Files metadata projection and every other selected read.
    /// Keeping that provenance at the controller boundary prevents a stale
    /// preview from participating in broader Files replacement lifecycles.
    pub(super) fn selected_onedrive_file_preview(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::OneDriveFilePreview { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::SelectedOneDriveFilePreview,
            preempted: false,
        }
    }

    /// A delivery refresh retains its original chat target independently of a
    /// later sidebar selection. The provider sees the same bounded interest,
    /// while the controller can keep selection cancellation from discarding a
    /// mutation's reconciliation read.
    pub(super) fn delivery_chat_messages_refresh(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatMessages { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::DeliveryChatMessagesRefresh,
            preempted: false,
        }
    }

    /// An ambiguous quote reply must be reconciled only by the exact bounded
    /// chat history that carries Graph's `messageReference` target, not by a
    /// directory preview or another post-delivery refresh for the same chat.
    pub(super) fn message_reconciliation(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChatMessages { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::MessageReconciliation,
            preempted: false,
        }
    }

    pub(super) fn delivery_channel_replies_refresh(interest: SyncInterest) -> Self {
        debug_assert!(matches!(interest, SyncInterest::ChannelReplies { .. }));
        Self {
            interest,
            purpose: RemotePollPurpose::DeliveryChannelRepliesRefresh,
            preempted: false,
        }
    }

    pub(super) fn kind(&self) -> InterestKind {
        InterestKind::from(&self.interest)
    }

    pub(super) fn priority(&self) -> RemotePollPriority {
        match self.kind() {
            InterestKind::Identity => RemotePollPriority::Identity,
            InterestKind::Chats
            | InterestKind::Teams
            | InterestKind::Calendar
            | InterestKind::Files
            | InterestKind::All => RemotePollPriority::Directory,
            InterestKind::ChatMessages
            | InterestKind::ChatPinnedMessages
            | InterestKind::ChatMembers
            | InterestKind::ChatMemberPresence
            | InterestKind::MessageSearch
            | InterestKind::ChannelMessages
            | InterestKind::ChannelFiles
            | InterestKind::OneDriveFilePreview
            | InterestKind::ChannelReplies => RemotePollPriority::Selected,
        }
    }

    #[cfg(test)]
    pub(super) fn already_preempted_for_test(mut self) -> Self {
        self.preempted = true;
        self
    }

    pub(super) const fn was_preempted(&self) -> bool {
        self.preempted
    }

    pub(super) fn as_interest(&self) -> &SyncInterest {
        &self.interest
    }

    pub(super) fn into_interest(self) -> SyncInterest {
        self.interest
    }

    pub(super) fn is_selected_channel_replies(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedChannelReplies
    }

    pub(super) fn is_selected_chat_messages(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedChatMessages
    }

    pub(super) fn is_selected_chat_pinned_messages(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedChatPinnedMessages
    }

    pub(super) fn is_selected_chat_members(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedChatMembers
    }

    pub(super) fn is_selected_chat_member_presence(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedChatMemberPresence
    }

    pub(super) fn is_selected_message_search(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedMessageSearch
    }

    pub(super) fn is_selected_onedrive_file_preview(&self) -> bool {
        self.purpose == RemotePollPurpose::SelectedOneDriveFilePreview
    }

    #[cfg(test)]
    pub(super) fn is_delivery_chat_messages_refresh(&self) -> bool {
        self.purpose == RemotePollPurpose::DeliveryChatMessagesRefresh
    }

    #[cfg(test)]
    pub(super) fn is_message_reconciliation(&self) -> bool {
        self.purpose == RemotePollPurpose::MessageReconciliation
    }

    pub(super) fn message_reconciliation_chat(&self) -> Option<ProviderResourceRef> {
        (self.purpose == RemotePollPurpose::MessageReconciliation).then(|| {
            let SyncInterest::ChatMessages { chat } = &self.interest else {
                unreachable!("quote reply reconciliation always reads chat messages")
            };
            chat.clone()
        })
    }

    #[cfg(test)]
    pub(super) fn is_delivery_channel_replies_refresh(&self) -> bool {
        self.purpose == RemotePollPurpose::DeliveryChannelRepliesRefresh
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RemoteWork {
    Poll(RemotePoll),
    Dispatch(WorkspaceIntentId),
}

pub(super) enum RemoteWorkResult {
    // Calendar snapshots can be materially larger than command receipts.
    // Keep the cross-thread completion enum compact so the dispatch path does
    // not inherit the largest projection's stack footprint.
    Poll(Box<Result<WorkspaceDelta, TransportError>>),
    Dispatch(Result<CommandReceipt, TransportError>),
}

pub(super) struct RemoteCompletion {
    pub(super) generation: u64,
    pub(super) work: RemoteWork,
    pub(super) transport: Box<dyn WorkspaceTransport>,
    pub(super) result: RemoteWorkResult,
}

impl WorkspaceTransportController {
    pub(super) fn start_remote_poll(&mut self, poll: RemotePoll) -> Result<(), TransportError> {
        let worker_interest = poll.as_interest().clone();
        self.start_remote_work(RemoteWork::Poll(poll), move |transport, cancellation| {
            RemoteWorkResult::Poll(Box::new(
                transport.poll_cancellable(worker_interest, cancellation),
            ))
        })
    }

    pub(super) fn start_remote_dispatch(
        &mut self,
        intent_id: WorkspaceIntentId,
        command: WorkspaceCommand,
    ) -> Result<(), TransportError> {
        self.start_remote_work(
            RemoteWork::Dispatch(intent_id),
            move |transport, cancellation| {
                RemoteWorkResult::Dispatch(transport.dispatch_cancellable(command, cancellation))
            },
        )
    }

    fn start_remote_work(
        &mut self,
        work: RemoteWork,
        operation: impl FnOnce(&mut dyn WorkspaceTransport, &TransportCancellation) -> RemoteWorkResult
        + Send
        + 'static,
    ) -> Result<(), TransportError> {
        let runtime = std::mem::replace(&mut self.runtime, TransportRuntime::Empty);
        let TransportRuntime::RemoteReady(mut transport) = runtime else {
            self.runtime = runtime;
            return Err(TransportError::Unavailable(
                "workspace transport was not ready to synchronize".into(),
            ));
        };

        let generation = self.remote_generation;
        let cancellation = TransportCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_work = work.clone();
        let waker = self.repaint_waker.clone();
        let spawn_result = std::thread::Builder::new()
            .name(remote_worker_name(&work).into())
            .spawn(move || {
                let result = operation(transport.as_mut(), &worker_cancellation);
                let _ = sender.send(RemoteCompletion {
                    generation,
                    work: worker_work,
                    transport,
                    result,
                });
                // After the send, so the UI's next `try_recv` sees the result.
                if let Some(waker) = waker {
                    waker();
                }
            });
        if spawn_result.is_err() {
            // The failed spawn consumes the remote adapter. Fall back safely
            // to the deterministic local preview instead of retaining a
            // partially initialized remote client or blocking the UI. Keep
            // the FIFO and correlation sequence so the caller can terminally
            // resolve the unstarted remote head and safely stage local tails.
            let error = TransportError::Unavailable(
                "could not start the Microsoft Graph synchronization worker".into(),
            );
            self.fallback_to_local_after_remote_loss(
                "could not start the Microsoft Graph synchronization worker",
                error.clone(),
            );
            return Err(error);
        }

        self.runtime = TransportRuntime::RemoteBusy {
            receiver,
            cancellation,
            generation,
            work,
        };
        Ok(())
    }

    pub(super) fn collect_remote_work(&mut self) {
        let runtime = std::mem::replace(&mut self.runtime, TransportRuntime::Empty);
        let TransportRuntime::RemoteBusy {
            receiver,
            cancellation,
            generation,
            work,
        } = runtime
        else {
            self.runtime = runtime;
            return;
        };

        match receiver.try_recv() {
            Ok(RemoteCompletion {
                generation: worker_generation,
                work,
                transport,
                result,
            }) => {
                debug_assert_eq!(generation, worker_generation);
                let status = transport.connection_status();
                self.runtime = TransportRuntime::RemoteReady(transport);
                let preempted = std::mem::take(&mut self.preempting_directory_poll);
                if preempted
                    && worker_generation == self.remote_generation
                    && let RemoteWork::Poll(mut poll) = work.clone()
                    && poll.priority() == RemotePollPriority::Directory
                {
                    // The poll was cancelled only to let a user command run.
                    // Its result is discarded, so put it back at the front
                    // exactly once; any equal queued poll is replaced.
                    poll.preempted = true;
                    self.pending_remote_interests
                        .retain(|pending| pending != &poll);
                    self.pending_remote_interests.push_front(poll);
                    return;
                }
                if cancellation.is_cancelled() || worker_generation != self.remote_generation {
                    // The result belongs to a profile/account that was
                    // superseded while its worker owned the adapter. Retain
                    // the returned adapter, but never surface its status,
                    // error, notice, identity, or projection.
                    return;
                }
                self.status = status;
                match (work, result) {
                    (RemoteWork::Poll(poll), RemoteWorkResult::Poll(result)) => {
                        if let Err(error) = self.handle_completed_remote_poll(poll, *result) {
                            self.pending_error = Some(error);
                        }
                    }
                    (RemoteWork::Dispatch(intent_id), RemoteWorkResult::Dispatch(result)) => {
                        self.handle_completed_remote_dispatch(intent_id, result);
                    }
                    _ => {
                        self.pending_error = Some(TransportError::Unavailable(
                            "workspace transport worker returned an invalid result".into(),
                        ));
                    }
                }
            }
            Err(TryRecvError::Empty) => {
                self.runtime = TransportRuntime::RemoteBusy {
                    receiver,
                    cancellation,
                    generation,
                    work,
                };
            }
            Err(TryRecvError::Disconnected) => {
                // A worker panic or unexpected disconnect must not strand the
                // UI in a connecting state. The adapter is no longer usable,
                // but preserve FIFO order and every existing completion so
                // local tails can still stage and remote tails can receive an
                // explicit terminal outcome.
                let error = TransportError::Unavailable(
                    "Microsoft Graph synchronization stopped unexpectedly".into(),
                );
                let active_dispatch = match work {
                    RemoteWork::Dispatch(intent_id)
                        if !cancellation.is_cancelled() && generation == self.remote_generation =>
                    {
                        Some(intent_id)
                    }
                    _ => None,
                };
                self.fallback_to_local_after_remote_loss(
                    "Microsoft Graph synchronization stopped unexpectedly",
                    error.clone(),
                );
                if let Some(intent_id) = active_dispatch {
                    self.resolve_lost_remote_dispatch(intent_id);
                }
                self.pending_error = Some(error);
            }
        }
    }

    /// Replaces a lost remote adapter without losing correlation with work the
    /// UI already accepted. Provider deltas/interests are intentionally
    /// dropped: a local runtime cannot safely apply or replay them.
    pub(super) fn fallback_to_local_after_remote_loss(
        &mut self,
        status_detail: &'static str,
        reconciliation_error: TransportError,
    ) {
        let initialized_profile = self.initialized_profile.take();
        let pending_commands = std::mem::take(&mut self.pending_commands);
        let pending_dispatches = std::mem::take(&mut self.pending_dispatches);
        let pending_message_recoveries = std::mem::take(&mut self.pending_message_reconciliations);
        let next_intent_sequence = self.next_intent_sequence;

        let mut fallback = Self::local_preview();
        fallback.initialized_profile = initialized_profile;
        fallback.remote_generation = self.remote_generation.wrapping_add(1);
        fallback.remote_projection_clear_requested = true;
        fallback.remote_projection_clear_for_transport_loss_requested = true;
        fallback.status = ConnectionStatus::Unavailable(status_detail.into());
        fallback.pending_commands = pending_commands;
        fallback.pending_dispatches = pending_dispatches;
        fallback.next_intent_sequence = next_intent_sequence;
        for pending in pending_message_recoveries {
            fallback
                .pending_dispatches
                .push_back(CommandDispatch::ReconciliationFailed {
                    intent_id: pending.intent_id,
                    error: reconciliation_error.clone(),
                });
        }
        fallback.repaint_waker = self.repaint_waker.take();
        *self = fallback;
    }

    /// The worker vanished while owning a non-idempotent POST. It may have
    /// reached Graph, so discard only this active head. Chat retains its
    /// existing immediately-failed reconciliation path; a channel post or
    /// root reply stays explicitly delivery-unknown because the lost adapter
    /// cannot safely prove its capped feed absence or resend it.
    pub(super) fn resolve_lost_remote_dispatch(&mut self, intent_id: WorkspaceIntentId) {
        let Some(command) = self
            .pending_commands
            .front()
            .filter(|queued| queued.intent_id == intent_id)
            .map(|queued| queued.command.clone())
        else {
            debug_assert!(false, "a lost remote worker must own the FIFO head");
            return;
        };
        let _ = self.pending_commands.pop_front();
        let error = TransportError::Unavailable(
            "Microsoft Graph message delivery stopped unexpectedly".into(),
        );
        self.pending_dispatches
            .push_back(CommandDispatch::DeliveryUncertain {
                intent_id,
                error: error.clone(),
            });
        if matches!(
            command,
            WorkspaceCommand::SendChatMessage { .. }
                | WorkspaceCommand::ReplyToChatMessageWithQuote { .. }
                | WorkspaceCommand::EditChatMessage { .. }
                | WorkspaceCommand::DeleteChatMessage { .. }
        ) {
            self.pending_dispatches
                .push_back(CommandDispatch::ReconciliationFailed { intent_id, error });
        }
    }

    /// Stages one local-preview command immediately. The public FIFO owns
    /// retry ordering; remote commands must go through a worker and therefore
    /// cannot be dispatched by this helper.
    #[cfg(test)]
    pub(super) fn dispatch_now(
        &mut self,
        command: WorkspaceCommand,
    ) -> Result<CommandReceipt, TransportError> {
        match command.target_provider()? {
            CommandTargetProvider::LocalPreview => self.dispatch_local_now(command),
            CommandTargetProvider::Microsoft365 => Err(TransportError::Unsupported(
                "Microsoft 365 commands must be dispatched by a remote worker".into(),
            )),
        }
    }

    pub(super) fn dispatch_local_now(
        &mut self,
        command: WorkspaceCommand,
    ) -> Result<CommandReceipt, TransportError> {
        match &mut self.runtime {
            TransportRuntime::Local(transport) => {
                let receipt = transport.dispatch(command);
                self.status = transport.connection_status();
                receipt
            }
            TransportRuntime::RemoteReady(_) | TransportRuntime::RemoteBusy { .. } => {
                self.local_command_transport.dispatch(command)
            }
            TransportRuntime::Empty => Err(TransportError::Unavailable(
                "workspace transport was not ready to accept a command".into(),
            )),
        }
    }
}

pub(super) fn remote_worker_name(work: &RemoteWork) -> &'static str {
    match work {
        RemoteWork::Poll(poll) => match poll.as_interest() {
            SyncInterest::Identity => "teamspace-graph-identity",
            SyncInterest::Chats => "teamspace-graph-chats",
            SyncInterest::ChatMessages { .. } => "teamspace-graph-chat-messages",
            SyncInterest::ChatPinnedMessages { .. } => "teamspace-graph-chat-pinned-messages",
            SyncInterest::ChatMembers { .. } => "teamspace-graph-chat-members",
            SyncInterest::ChatMemberPresence { .. } => "teamspace-graph-chat-presence",
            SyncInterest::MessageSearch { .. } => "teamspace-graph-message-search",
            SyncInterest::Teams => "teamspace-graph-teams",
            // Provider resource identifiers must never enter a thread name.
            // The selection itself remains inside the opaque SyncInterest value.
            SyncInterest::ChannelMessages { .. } => "teamspace-graph-channel-messages",
            SyncInterest::ChannelFiles { .. } => "teamspace-graph-channel-files",
            SyncInterest::OneDriveFilePreview { .. } => "teamspace-graph-onedrive-file-preview",
            SyncInterest::ChannelReplies { .. } => "teamspace-graph-channel-replies",
            SyncInterest::Calendar => "teamspace-graph-calendar",
            SyncInterest::Files => "teamspace-graph-files",
            SyncInterest::All => "teamspace-graph-sync",
        },
        RemoteWork::Dispatch(_) => "teamspace-graph-dispatch",
    }
}
