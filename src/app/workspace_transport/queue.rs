use super::WorkspaceTransportController;
use super::remote_worker::{RemoteWork, TransportRuntime};
use crate::integrations::{
    CommandReceipt, CommandTargetProvider, TransportError, WorkspaceCommand, WorkspaceIntentId,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(super) const COMMAND_RETRY_BASE: Duration = Duration::from_secs(2);
pub(super) const COMMAND_RETRY_MAX: Duration = Duration::from_secs(60);

/// A feature mutation accepted locally but not yet staged by the transport.
///
/// The queue is intentionally session-local. Local-preview targets may stage
/// synchronously, while remote targets remain at the FIFO head until their
/// worker returns. A durable remote outbox would additionally need provider
/// idempotency and atomic coordination with feature snapshots before it could
/// make stronger delivery guarantees.
pub(super) struct QueuedCommand {
    pub(super) intent_id: WorkspaceIntentId,
    pub(super) command: WorkspaceCommand,
    pub(super) attempts: u32,
    pub(super) next_attempt_at: Instant,
    pub(super) error_reported: bool,
}

/// The app-visible outcome of flushing due workspace intents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandDispatch {
    Delivered {
        intent_id: WorkspaceIntentId,
        receipt: CommandReceipt,
    },
    RetryScheduled {
        intent_id: WorkspaceIntentId,
        error: TransportError,
        notify_user: bool,
    },
    Rejected {
        intent_id: WorkspaceIntentId,
        error: TransportError,
    },
    /// The adapter could not determine whether a remote non-idempotent POST
    /// reached its provider. The command is intentionally removed from the
    /// FIFO instead of replaying it; when the remote runtime remains
    /// available, a command-specific best-effort projection refresh may be
    /// scheduled. A lost worker cannot schedule that refresh.
    DeliveryUncertain {
        intent_id: WorkspaceIntentId,
        error: TransportError,
    },
    /// The exact-chat history read has finished after an uncertain remote
    /// send. The feature can now decide whether that history confirms the
    /// original text or restore it for an explicit user retry.
    ReconciliationCompleted { intent_id: WorkspaceIntentId },
    /// The exact-chat history needed to reconcile an ambiguous remote send
    /// could not be read. This never replays the non-idempotent POST.
    ReconciliationFailed {
        intent_id: WorkspaceIntentId,
        error: TransportError,
    },
}

impl WorkspaceTransportController {
    /// Cancels remote intents whose projected chat disappeared before their
    /// POST began. The feature keeps a session-only tombstone for each ID, so
    /// the resulting terminal outcome can restore its draft if that chat
    /// returns in a later bounded projection. An in-flight POST is left alone:
    /// cancellation cannot prove whether a request already reached Graph.
    pub(crate) fn cancel_queued_remote_intents(
        &mut self,
        intent_ids: impl IntoIterator<Item = WorkspaceIntentId>,
    ) {
        let intent_ids = intent_ids.into_iter().collect::<Vec<_>>();
        if intent_ids.is_empty() {
            return;
        }
        let active_remote_dispatch = match &self.runtime {
            TransportRuntime::RemoteBusy {
                work: RemoteWork::Dispatch(intent_id),
                ..
            } => Some(*intent_id),
            _ => None,
        };
        let mut retained = VecDeque::with_capacity(self.pending_commands.len());
        while let Some(queued) = self.pending_commands.pop_front() {
            let is_requested = intent_ids.contains(&queued.intent_id);
            let is_unstarted_remote = active_remote_dispatch != Some(queued.intent_id)
                && matches!(
                    queued.command.target_provider(),
                    Ok(CommandTargetProvider::Microsoft365)
                );
            if is_requested && is_unstarted_remote {
                self.pending_dispatches
                    .push_back(CommandDispatch::Rejected {
                        intent_id: queued.intent_id,
                        error: TransportError::Cancelled,
                    });
            } else {
                retained.push_back(queued);
            }
        }
        self.pending_commands = retained;
    }

    /// Adds accepted feature intents to the session-local FIFO and returns
    /// opaque correlation keys in the same order. A transient dispatch
    /// failure never lets a later command overtake its predecessor.
    pub(crate) fn enqueue(
        &mut self,
        commands: impl IntoIterator<Item = WorkspaceCommand>,
    ) -> Vec<WorkspaceIntentId> {
        let now = Instant::now();
        commands
            .into_iter()
            .map(|command| {
                let intent_id = self.allocate_intent_id();
                self.pending_commands.push_back(QueuedCommand {
                    intent_id,
                    command,
                    attempts: 0,
                    next_attempt_at: now,
                    error_reported: false,
                });
                intent_id
            })
            .collect()
    }

    fn allocate_intent_id(&mut self) -> WorkspaceIntentId {
        let sequence = self.next_intent_sequence;
        self.next_intent_sequence = self.next_intent_sequence.checked_add(1).unwrap_or(1);
        WorkspaceIntentId::from_sequence(sequence)
    }

    /// Flushes all commands currently due until one transient failure blocks
    /// the FIFO. Invalid commands are discarded rather than permanently
    /// blocking later accepted local mutations; features should never emit
    /// those because their local validation runs first.
    pub(crate) fn dispatch_due(&mut self) -> Vec<CommandDispatch> {
        self.dispatch_due_at(Instant::now())
    }

    pub(super) fn dispatch_due_at(&mut self, now: Instant) -> Vec<CommandDispatch> {
        self.collect_remote_work();
        let mut outcomes = self.pending_dispatches.drain(..).collect::<Vec<_>>();

        // A remote receipt or uncertain outcome can schedule a bounded,
        // command-targeted refresh. Selection/delivery reads start before
        // another remote intent so the projection catches up promptly;
        // directory/bootstrap reads yield to a due command (see
        // `start_next_remote_poll_at`).
        if matches!(&self.runtime, TransportRuntime::RemoteReady(_))
            && !self.pending_remote_interests.is_empty()
            && let Err(error) = self.start_next_remote_poll_at(now)
        {
            self.pending_error = Some(error);
            outcomes.extend(self.pending_dispatches.drain(..));
        }

        while let Some((intent_id, command)) = self.pending_commands.front().and_then(|queued| {
            (queued.next_attempt_at <= now).then(|| (queued.intent_id, queued.command.clone()))
        }) {
            let provider = match command.target_provider() {
                Ok(provider) => provider,
                Err(error) => {
                    let _ = self.pending_commands.pop_front();
                    outcomes.push(CommandDispatch::Rejected { intent_id, error });
                    continue;
                }
            };

            match provider {
                CommandTargetProvider::LocalPreview => match self.dispatch_local_now(command) {
                    Ok(receipt) => {
                        let _ = self.pending_commands.pop_front();
                        outcomes.push(CommandDispatch::Delivered { intent_id, receipt });
                    }
                    Err(error @ TransportError::InvalidCommand(_)) => {
                        let _ = self.pending_commands.pop_front();
                        outcomes.push(CommandDispatch::Rejected { intent_id, error });
                    }
                    Err(error) => match self.schedule_retry(intent_id, &error, now) {
                        Ok(Some(notify_user)) => {
                            outcomes.push(CommandDispatch::RetryScheduled {
                                intent_id,
                                error,
                                notify_user,
                            });
                            break;
                        }
                        Ok(None) => {
                            debug_assert!(false, "a local retry must retain the FIFO head");
                            break;
                        }
                        Err(rejection) => {
                            let _ = self.pending_commands.pop_front();
                            outcomes.push(CommandDispatch::Rejected {
                                intent_id,
                                error: rejection,
                            });
                        }
                    },
                },
                CommandTargetProvider::Microsoft365 => {
                    if matches!(&self.runtime, TransportRuntime::RemoteBusy { .. }) {
                        // The remote FIFO head remains queued until its worker
                        // returns. A later local intent cannot overtake it. A
                        // running directory/bootstrap poll is asked to stop
                        // so the command waits for one request, not a fan-out.
                        self.preempt_directory_poll_for_command();
                        break;
                    }
                    if matches!(&self.runtime, TransportRuntime::RemoteReady(_)) {
                        match self.start_remote_dispatch(intent_id, command) {
                            Ok(()) => break,
                            Err(error) => {
                                // The worker could not start, so no request was
                                // issued. Do not downgrade this M365 target to
                                // a local preview command.
                                if self
                                    .pending_commands
                                    .front()
                                    .is_some_and(|queued| queued.intent_id == intent_id)
                                {
                                    let _ = self.pending_commands.pop_front();
                                }
                                outcomes.push(CommandDispatch::Rejected { intent_id, error });
                                // A failed worker spawn converted the runtime
                                // to Local, so finish resolving remote tails
                                // and stage any local commands in FIFO order.
                                continue;
                            }
                        }
                    }
                    let _ = self.pending_commands.pop_front();
                    outcomes.push(CommandDispatch::Rejected {
                        intent_id,
                        error: TransportError::Unsupported(
                            "a Microsoft 365 connection is required for this command".into(),
                        ),
                    });
                }
            }
        }
        outcomes
    }

    /// Requests a future egui frame only when something is waiting for a
    /// deadline: the FIFO head's retry, or a queued read deferred by a
    /// Retry-After cooldown. This avoids retrying on every paint.
    pub(crate) fn next_retry_delay(&self) -> Option<Duration> {
        let now = Instant::now();
        let command = self.pending_commands.front().and_then(|queued| {
            (queued.attempts > 0).then(|| queued.next_attempt_at.saturating_duration_since(now))
        });
        let cooldown = self.next_poll_cooldown_delay_at(now);
        match (command, cooldown) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        }
    }

    #[cfg(test)]
    pub(super) fn pending_command_count(&self) -> usize {
        self.pending_commands.len()
    }

    fn schedule_retry(
        &mut self,
        intent_id: WorkspaceIntentId,
        error: &TransportError,
        now: Instant,
    ) -> Result<Option<bool>, TransportError> {
        let Some(queued) = self.pending_commands.front_mut() else {
            return Ok(None);
        };
        if queued.intent_id != intent_id {
            return Ok(None);
        }
        let attempts = queued.attempts.saturating_add(1);
        let delay = command_retry_delay(error, attempts);
        let Some(next_attempt_at) = now.checked_add(delay) else {
            return Err(TransportError::Unavailable(
                "the provider retry deadline is too far in the future; retry the message manually"
                    .into(),
            ));
        };
        queued.attempts = attempts;
        queued.next_attempt_at = next_attempt_at;
        let notify_user = !queued.error_reported;
        queued.error_reported = true;
        Ok(Some(notify_user))
    }

    pub(super) fn handle_completed_remote_dispatch(
        &mut self,
        intent_id: WorkspaceIntentId,
        result: Result<CommandReceipt, TransportError>,
    ) {
        let Some(queued) = self.pending_commands.front() else {
            debug_assert!(false, "a remote completion must have a queued command");
            return;
        };
        if queued.intent_id != intent_id {
            debug_assert!(
                false,
                "a remote completion must match the FIFO command head"
            );
            return;
        }
        // The result has one FIFO head, but follow-up reads must remain
        // command-specific. A successful/ambiguous chat-message reaction
        // refreshes only its exact ChatMessages target; a channel post, root
        // reply, or channel reaction must never refresh Chats merely because
        // chat sends were the first remote mutation supported by this
        // controller.
        let command = queued.command.clone();

        match result {
            Ok(receipt) => {
                let _ = self.pending_commands.pop_front();
                self.queue_remote_refresh_after_send(&command);
                self.pending_dispatches
                    .push_back(CommandDispatch::Delivered { intent_id, receipt });
            }
            Err(TransportError::Cancelled) => {
                // Account/profile replacement clears the FIFO before the
                // worker can return. Do not resurrect an old intent or show a
                // failure that belongs to a superseded session.
            }
            // A read receipt is best-effort and never replayed, even after a
            // provider-confirmed throttle: the next view sends the next one.
            // An upload (the file PUT, the session's creation, a fragment or
            // its cancellation) is a write that is never queued for replay
            // either: a throttle becomes a terminal rejection, the composer
            // keeps its draft and the user picks the file again.
            Err(error @ TransportError::Throttled { .. })
                if matches!(
                    command,
                    WorkspaceCommand::MarkChatRead { .. }
                        | WorkspaceCommand::UploadChatFile { .. }
                        | WorkspaceCommand::ContinueChatUpload { .. }
                        | WorkspaceCommand::CancelChatUpload { .. }
                        | WorkspaceCommand::ShareUploadedFileWithChat { .. }
                ) =>
            {
                let _ = self.pending_commands.pop_front();
                self.pending_dispatches
                    .push_back(CommandDispatch::Rejected { intent_id, error });
            }
            Err(error @ TransportError::Throttled { .. }) => {
                match self.schedule_retry(intent_id, &error, Instant::now()) {
                    Ok(Some(notify_user)) => {
                        self.pending_dispatches
                            .push_back(CommandDispatch::RetryScheduled {
                                intent_id,
                                error,
                                notify_user,
                            });
                    }
                    Ok(None) => {
                        debug_assert!(false, "a remote retry must retain its FIFO head");
                    }
                    Err(rejection) => {
                        let _ = self.pending_commands.pop_front();
                        self.pending_dispatches
                            .push_back(CommandDispatch::Rejected {
                                intent_id,
                                error: rejection,
                            });
                    }
                }
            }
            Err(error @ TransportError::Unavailable(_)) => {
                // A timeout or 5xx after a non-idempotent POST is ambiguous:
                // retrying can duplicate a message. Drop it from automatic
                // replay. Chat and quote-reply sends reconcile
                // only against a refresh of that exact chat; reaction, channel post, and reply refreshes are
                // deliberately best-effort presentation only because an
                // absent capped feed row cannot prove that the mutation was
                // not accepted.
                let _ = self.pending_commands.pop_front();
                match &command {
                    WorkspaceCommand::SendChatMessage { chat, .. } => {
                        self.queue_remote_message_reconciliation(intent_id, chat.clone());
                    }
                    WorkspaceCommand::ReplyToChatMessageWithQuote { chat, .. }
                    | WorkspaceCommand::EditChatMessage { chat, .. }
                    | WorkspaceCommand::DeleteChatMessage { chat, .. } => {
                        // An edit or deletion is confirmed or refuted only
                        // by a fresh read of that exact chat, never replayed.
                        self.queue_remote_message_reconciliation(intent_id, chat.clone());
                    }
                    WorkspaceCommand::SetChatMessageReaction { chat, .. } => {
                        self.queue_remote_chat_messages_refresh(chat.clone());
                    }
                    WorkspaceCommand::SendChannelPost { team, channel, .. } => {
                        self.queue_remote_channel_messages_refresh(team.clone(), channel.clone());
                    }
                    WorkspaceCommand::ReplyToChannelPost {
                        team,
                        channel,
                        post,
                        ..
                    } => {
                        self.queue_remote_channel_replies_refresh(
                            team.clone(),
                            channel.clone(),
                            post.clone(),
                        );
                    }
                    WorkspaceCommand::SetChannelMessageReaction { team, channel, .. } => {
                        self.queue_remote_channel_messages_refresh(team.clone(), channel.clone());
                    }
                    WorkspaceCommand::SetChannelMessageReplyReaction {
                        team,
                        channel,
                        post,
                        ..
                    } => {
                        self.queue_remote_channel_replies_refresh(
                            team.clone(),
                            channel.clone(),
                            post.clone(),
                        );
                    }
                    _ => {}
                }
                self.pending_dispatches
                    .push_back(CommandDispatch::DeliveryUncertain { intent_id, error });
            }
            Err(error) => {
                let _ = self.pending_commands.pop_front();
                self.pending_dispatches
                    .push_back(CommandDispatch::Rejected { intent_id, error });
            }
        }
    }

    /// Schedules only the bounded projection that can show a remote mutation
    /// after Graph accepted it. This does not add local records or infer
    /// delivery from a later snapshot.
    fn queue_remote_refresh_after_send(&mut self, command: &WorkspaceCommand) {
        match command {
            WorkspaceCommand::SendChatMessage { .. } => self.queue_remote_chat_refresh(),
            WorkspaceCommand::ReplyToChatMessageWithQuote { chat, .. }
            | WorkspaceCommand::EditChatMessage { chat, .. }
            | WorkspaceCommand::DeleteChatMessage { chat, .. } => {
                self.queue_remote_chat_messages_refresh(chat.clone());
            }
            WorkspaceCommand::SetChatMessageReaction { chat, .. } => {
                self.queue_remote_chat_messages_refresh(chat.clone());
            }
            WorkspaceCommand::SendChannelPost { team, channel, .. } => {
                self.queue_remote_channel_messages_refresh(team.clone(), channel.clone());
            }
            WorkspaceCommand::ReplyToChannelPost {
                team,
                channel,
                post,
                ..
            } => {
                self.queue_remote_channel_replies_refresh(
                    team.clone(),
                    channel.clone(),
                    post.clone(),
                );
            }
            WorkspaceCommand::SetChannelMessageReaction { team, channel, .. } => {
                self.queue_remote_channel_messages_refresh(team.clone(), channel.clone());
            }
            WorkspaceCommand::SetChannelMessageReplyReaction {
                team,
                channel,
                post,
                ..
            } => {
                self.queue_remote_channel_replies_refresh(
                    team.clone(),
                    channel.clone(),
                    post.clone(),
                );
            }
            _ => {}
        }
    }

    fn queue_remote_message_reconciliation(
        &mut self,
        intent_id: WorkspaceIntentId,
        chat: crate::integrations::ProviderResourceRef,
    ) {
        if !self
            .pending_message_reconciliations
            .iter()
            .any(|pending| pending.intent_id == intent_id && pending.chat == chat)
        {
            self.pending_message_reconciliations
                .push_back(super::PendingMessageReconciliation {
                    intent_id,
                    chat: chat.clone(),
                });
        }
        self.queue_remote_poll(super::remote_worker::RemotePoll::message_reconciliation(
            crate::integrations::SyncInterest::ChatMessages { chat },
        ));
    }

    pub(super) fn complete_pending_message_reconciliations(
        &mut self,
        chat: crate::integrations::ProviderResourceRef,
    ) {
        let mut remaining = VecDeque::new();
        while let Some(pending) = self.pending_message_reconciliations.pop_front() {
            if pending.chat == chat {
                self.pending_dispatches
                    .push_back(CommandDispatch::ReconciliationCompleted {
                        intent_id: pending.intent_id,
                    });
            } else {
                remaining.push_back(pending);
            }
        }
        self.pending_message_reconciliations = remaining;
    }

    pub(super) fn fail_pending_message_reconciliations(
        &mut self,
        chat: crate::integrations::ProviderResourceRef,
        error: TransportError,
    ) {
        let mut remaining = VecDeque::new();
        while let Some(pending) = self.pending_message_reconciliations.pop_front() {
            if pending.chat == chat {
                self.pending_dispatches
                    .push_back(CommandDispatch::ReconciliationFailed {
                        intent_id: pending.intent_id,
                        error: error.clone(),
                    });
            } else {
                remaining.push_back(pending);
            }
        }
        self.pending_message_reconciliations = remaining;
    }
}

pub(super) fn command_retry_delay(error: &TransportError, attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(5);
    let backoff = COMMAND_RETRY_BASE
        .saturating_mul(1_u32 << exponent)
        .min(COMMAND_RETRY_MAX);
    match error {
        TransportError::Throttled {
            retry_after_seconds: Some(seconds),
        } => backoff.max(Duration::from_secs(*seconds)),
        _ => backoff,
    }
}
