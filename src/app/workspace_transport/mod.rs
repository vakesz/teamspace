//! App-owned coordination for the replaceable workspace transport.
//!
//! Features mutate their local projections first. This controller stages the
//! matching provider-independent intent through the configured adapter and
//! keeps account/connection details out of feature modules. Remote
//! synchronization requests are always moved off the egui update thread.

use crate::integrations::{
    CommandDisposition, CommandReceipt, ConnectionStatus, DelegatedTokens, LocalPreviewTransport,
    Microsoft365Transport, ProviderResourceRef, SyncInterest, TransportError, WorkspaceDelta,
    WorkspaceIntentId, WorkspaceTransport,
};
use std::collections::VecDeque;

mod polling;
mod queue;
mod remote_worker;

use polling::PollCooldown;
pub(crate) use queue::CommandDispatch;
use queue::QueuedCommand;
use remote_worker::{InterestKind, RemotePoll, TransportRuntime};

/// A thread-safe "request a repaint" callback installed by the app shell.
pub(crate) type RepaintWaker = std::sync::Arc<dyn Fn() + Send + Sync>;

pub(crate) struct WorkspaceTransportController {
    runtime: TransportRuntime,
    /// Retains deterministic local-preview behavior while a remote adapter
    /// owns Microsoft 365 projections. Provider routing is decided from opaque
    /// targets, so a remote command can never be downgraded to a local stage.
    local_command_transport: LocalPreviewTransport,
    initialized_profile: Option<String>,
    status: ConnectionStatus,
    pending_notice: Option<String>,
    pending_error: Option<TransportError>,
    /// Monotonically advances whenever remote state is invalidated. A worker
    /// returns its generation with the adapter so a late result can be safely
    /// discarded without losing adapter ownership.
    remote_generation: u64,
    /// One-shot signal consumed by the app before it applies new deltas. It
    /// ensures feature-owned remote projections are cleared as soon as an
    /// account/profile transition begins, rather than after an old worker
    /// finally returns.
    remote_projection_clear_requested: bool,
    /// Narrows a projection clear to unexpected adapter loss. The app still
    /// clears stale views, but can retain already-bound remote channel-post
    /// and root-reply composers long enough to map their terminal outcomes.
    /// Explicit disconnects and profile/account transitions always use the
    /// ordinary clear.
    remote_projection_clear_for_transport_loss_requested: bool,
    /// Remote poll requests are serialized through one worker because the
    /// adapter owns connection state. Identity always runs first, then
    /// selection and delivery reads, then account-wide directory
    /// projections (Chats, Teams, Calendar, Files); see
    /// `RemotePollPriority`. Order is FIFO within a tier.
    pending_remote_interests: VecDeque<RemotePoll>,
    /// A server-directed Retry-After embargo per interest kind (Chats, Teams,
    /// ChatMessages, ...). A request made during the embargo is queued as
    /// usual but not started until it expires; nothing is dropped and the
    /// failed read itself is never replayed automatically. Identity is
    /// exempt because every other projection depends on it.
    poll_cooldowns: std::collections::BTreeMap<InterestKind, PollCooldown>,
    /// Set while a running directory poll was cancelled to let a due user
    /// command through; `collect_remote_work` then re-queues that poll once.
    preempting_directory_poll: bool,
    /// Called from a worker thread right after it hands its result back, so
    /// the UI repaints when work completes instead of polling on a timer.
    /// It survives adapter replacement and loss.
    repaint_waker: Option<RepaintWaker>,
    /// Complete normalized deltas waiting for the app to apply on the UI
    /// thread. Features never communicate with provider adapters directly.
    pending_deltas: VecDeque<WorkspaceDelta>,
    /// Ambiguous remote sends and quote replies wait for an exact-chat
    /// `ChatMessages` read before their feature-owned composers may be made
    /// editable again. Keep the chat alongside each intent so another
    /// conversation's refresh cannot complete it; a quote reply additionally
    /// needs history that still carries its exact opaque quote target.
    pending_message_reconciliations: VecDeque<PendingMessageReconciliation>,
    pending_commands: VecDeque<QueuedCommand>,
    next_intent_sequence: u64,
    /// Worker completions are gathered during profile synchronization but
    /// reported to feature code from `dispatch_due`, preserving the existing
    /// app-facing flush point.
    pending_dispatches: VecDeque<CommandDispatch>,
}

struct PendingMessageReconciliation {
    intent_id: WorkspaceIntentId,
    chat: ProviderResourceRef,
}

impl Default for WorkspaceTransportController {
    fn default() -> Self {
        #[cfg(test)]
        {
            Self::local_preview()
        }
        #[cfg(not(test))]
        {
            Self::from_environment()
        }
    }
}

impl WorkspaceTransportController {
    #[allow(dead_code)] // Called by the production default; tests force local preview.
    pub(super) fn from_environment() -> Self {
        match Microsoft365Transport::from_environment() {
            Ok(Some(transport)) => Self::microsoft365(transport),
            Ok(None) => Self::local_preview(),
            Err(error) => {
                let mut controller = Self::local_preview();
                controller.status = ConnectionStatus::Unavailable(
                    "Microsoft Graph configuration is invalid; running the local preview instead"
                        .into(),
                );
                controller.pending_error = Some(error);
                controller
            }
        }
    }

    pub(super) fn local_preview() -> Self {
        let transport: Box<dyn WorkspaceTransport> = Box::new(LocalPreviewTransport::default());
        let status = transport.connection_status();
        Self {
            runtime: TransportRuntime::Local(transport),
            local_command_transport: LocalPreviewTransport::default(),
            initialized_profile: None,
            status,
            pending_notice: None,
            pending_error: None,
            remote_generation: 0,
            remote_projection_clear_requested: false,
            remote_projection_clear_for_transport_loss_requested: false,
            pending_remote_interests: VecDeque::new(),
            poll_cooldowns: std::collections::BTreeMap::new(),
            preempting_directory_poll: false,
            repaint_waker: None,
            pending_deltas: VecDeque::new(),
            pending_message_reconciliations: VecDeque::new(),
            pending_commands: VecDeque::new(),
            next_intent_sequence: 1,
            pending_dispatches: VecDeque::new(),
        }
    }

    fn microsoft365(transport: Microsoft365Transport) -> Self {
        let transport: Box<dyn WorkspaceTransport> = Box::new(transport);
        let status = transport.connection_status();
        Self {
            runtime: TransportRuntime::RemoteReady(transport),
            local_command_transport: LocalPreviewTransport::default(),
            initialized_profile: None,
            status,
            pending_notice: None,
            pending_error: None,
            remote_generation: 0,
            remote_projection_clear_requested: false,
            remote_projection_clear_for_transport_loss_requested: false,
            pending_remote_interests: VecDeque::new(),
            poll_cooldowns: std::collections::BTreeMap::new(),
            preempting_directory_poll: false,
            repaint_waker: None,
            pending_deltas: VecDeque::new(),
            pending_message_reconciliations: VecDeque::new(),
            pending_commands: VecDeque::new(),
            next_intent_sequence: 1,
            pending_dispatches: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_local_transport(transport: Box<dyn WorkspaceTransport>) -> Self {
        let status = transport.connection_status();
        Self {
            runtime: TransportRuntime::Local(transport),
            local_command_transport: LocalPreviewTransport::default(),
            initialized_profile: None,
            status,
            pending_notice: None,
            pending_error: None,
            remote_generation: 0,
            remote_projection_clear_requested: false,
            remote_projection_clear_for_transport_loss_requested: false,
            pending_remote_interests: VecDeque::new(),
            poll_cooldowns: std::collections::BTreeMap::new(),
            preempting_directory_poll: false,
            repaint_waker: None,
            pending_deltas: VecDeque::new(),
            pending_message_reconciliations: VecDeque::new(),
            pending_commands: VecDeque::new(),
            next_intent_sequence: 1,
            pending_dispatches: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_remote_transport(transport: Box<dyn WorkspaceTransport>) -> Self {
        let status = transport.connection_status();
        Self {
            runtime: TransportRuntime::RemoteReady(transport),
            local_command_transport: LocalPreviewTransport::default(),
            initialized_profile: None,
            status,
            pending_notice: None,
            pending_error: None,
            remote_generation: 0,
            remote_projection_clear_requested: false,
            remote_projection_clear_for_transport_loss_requested: false,
            pending_remote_interests: VecDeque::new(),
            poll_cooldowns: std::collections::BTreeMap::new(),
            preempting_directory_poll: false,
            repaint_waker: None,
            pending_deltas: VecDeque::new(),
            pending_message_reconciliations: VecDeque::new(),
            pending_commands: VecDeque::new(),
            next_intent_sequence: 1,
            pending_dispatches: VecDeque::new(),
        }
    }

    /// Atomically replaces the active adapter with an Entra-authenticated
    /// Microsoft 365 transport.
    ///
    /// The opaque token bundle is consumed before the old runtime is touched,
    /// so a construction failure retains the existing session. On success,
    /// prior identity synchronization, notices, errors, and FIFO commands are
    /// intentionally discarded: local commands must never be replayed against
    /// a newly connected account.
    pub(crate) fn connect_microsoft365(
        &mut self,
        tokens: DelegatedTokens,
    ) -> Result<(), TransportError> {
        let transport = Microsoft365Transport::from_delegated_tokens(tokens)?;
        let next_generation = self.remote_generation.wrapping_add(1);
        self.cancel_active_remote_work();
        let mut replacement = Self::microsoft365(transport);
        replacement.remote_generation = next_generation;
        replacement.remote_projection_clear_requested = true;
        replacement.repaint_waker = self.repaint_waker.take();
        *self = replacement;
        Ok(())
    }

    /// Discards any Microsoft 365 runtime and returns to a fresh local
    /// preview. This deliberately does not consult environment configuration,
    /// which prevents a disconnect or sign-out from silently reconnecting a
    /// process-injected development token.
    pub(crate) fn disconnect_microsoft365(&mut self) {
        let next_generation = self.remote_generation.wrapping_add(1);
        let clear_remote_projection =
            self.is_microsoft365_active() || self.remote_projection_clear_requested;
        self.cancel_active_remote_work();
        let mut replacement = Self::local_preview();
        replacement.remote_generation = next_generation;
        replacement.remote_projection_clear_requested = clear_remote_projection;
        replacement.repaint_waker = self.repaint_waker.take();
        *self = replacement;
    }

    /// Installs the callback a worker uses to wake the UI when it finishes.
    pub(crate) fn set_repaint_waker(&mut self, waker: RepaintWaker) {
        self.repaint_waker = Some(waker);
    }

    /// Drives the initial remote synchronization sequence for an authenticated
    /// profile.
    ///
    /// Local preview synchronization completes immediately. A configured
    /// Microsoft Graph adapter is moved to one background worker at a time;
    /// callers keep invoking this during normal frames to collect results and
    /// advance from identity to provider projections. Each failure is
    /// surfaced once per profile/session rather than on every frame. Resetting
    /// or signing in again creates an explicit retry point.
    pub(crate) fn synchronize_profile(&mut self, profile: &str) -> Result<(), TransportError> {
        let profile_changed = self.initialized_profile.as_deref() != Some(profile);
        if profile_changed {
            // Do this before collecting a worker result. If an earlier
            // account/profile poll finished between frames, its generation is
            // now stale and can never be applied under the new profile.
            self.begin_profile_transition(profile);
        }

        self.collect_remote_work();

        if profile_changed && matches!(&self.runtime, TransportRuntime::Local(_)) {
            let TransportRuntime::Local(transport) = &mut self.runtime else {
                unreachable!("the local runtime was checked immediately before polling");
            };
            let result = transport.poll(SyncInterest::Identity);
            self.status = transport.connection_status();
            self.handle_completed_poll(SyncInterest::Identity, result)?;
        }

        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }

        match &self.runtime {
            TransportRuntime::Local(_) | TransportRuntime::RemoteBusy { .. } => Ok(()),
            TransportRuntime::RemoteReady(_) => self.start_next_remote_poll(),
            TransportRuntime::Empty => Err(TransportError::Unavailable(
                "workspace transport was not ready to synchronize".into(),
            )),
        }
    }

    /// Invalidates work owned by a prior authenticated profile before any
    /// completed worker result can be collected. The adapter remains in its
    /// existing worker until it returns, so no concurrent access to its token
    /// or connection state is introduced.
    pub(super) fn begin_profile_transition(&mut self, profile: &str) {
        self.initialized_profile = Some(profile.into());
        // A profile boundary is always destructive. If an unexpected adapter
        // loss queued a preserve-recovery clear just before the account
        // changed, downgrade it to the ordinary clear so no remote draft can
        // cross profile boundaries.
        self.remote_projection_clear_for_transport_loss_requested = false;
        self.pending_remote_interests.clear();
        self.poll_cooldowns.clear();
        self.preempting_directory_poll = false;
        self.pending_deltas.clear();
        self.pending_notice = None;
        self.pending_error = None;
        self.pending_dispatches.clear();
        self.pending_message_reconciliations.clear();
        // A profile transition is a session boundary. Commands accepted for a
        // prior profile must not be staged after a new account becomes active.
        self.pending_commands.clear();

        if self.is_microsoft365_active() {
            self.remote_generation = self.remote_generation.wrapping_add(1);
            self.cancel_active_remote_work();
            self.status = ConnectionStatus::Authorizing;
            self.remote_projection_clear_requested = true;
            // A remote projection is meaningful only after the fresh identity
            // result completes. This interest remains queued while an old
            // worker cooperatively winds down and returns the adapter.
            self.pending_remote_interests
                .push_back(RemotePoll::interest(SyncInterest::Identity));
        }
    }

    fn cancel_active_remote_work(&mut self) {
        if let TransportRuntime::RemoteBusy { cancellation, .. } = &self.runtime {
            cancellation.cancel();
        }
    }

    /// Drains provider-normalized synchronization results for the app to
    /// apply on the UI thread. The controller never gives feature modules a
    /// live provider adapter or a provider response DTO.
    pub(crate) fn take_deltas(&mut self) -> Vec<WorkspaceDelta> {
        self.pending_deltas.drain(..).collect()
    }

    /// Returns and clears the request to remove the feature-owned remote
    /// projection before applying later synchronization deltas. The app owns
    /// the projection, so the controller communicates only this lifecycle
    /// fact instead of reaching into feature state directly.
    pub(crate) fn take_remote_projection_clear_requested(&mut self) -> bool {
        std::mem::take(&mut self.remote_projection_clear_requested)
    }

    /// Returns whether the pending projection clear is caused by an
    /// unexpected transport loss. Consume this only with the normal clear
    /// signal: the app uses it to preserve an already-bound remote channel
    /// post or root reply for its terminal outcome, not to retain a remote
    /// account across an explicit disconnect or profile change.
    pub(crate) fn take_remote_projection_clear_for_transport_loss_requested(&mut self) -> bool {
        std::mem::take(&mut self.remote_projection_clear_for_transport_loss_requested)
    }

    pub(crate) fn is_polling(&self) -> bool {
        matches!(self.runtime, TransportRuntime::RemoteBusy { .. })
    }

    /// Reports whether this session currently owns a Microsoft 365 runtime.
    /// It remains true while the identity worker temporarily owns that runtime
    /// so account controls do not flicker from Disconnect back to Connect.
    pub(crate) fn is_microsoft365_active(&self) -> bool {
        matches!(
            &self.runtime,
            TransportRuntime::RemoteReady(_) | TransportRuntime::RemoteBusy { .. }
        )
    }

    /// The connected Microsoft 365 account's display name, when connected.
    pub(crate) fn connected_account_name(&self) -> Option<&str> {
        match &self.status {
            ConnectionStatus::Connected(identity) => Some(&identity.display_name),
            _ => None,
        }
    }

    pub(crate) fn status_label(&self) -> String {
        match &self.status {
            ConnectionStatus::LocalPreview => "Local preview".into(),
            ConnectionStatus::Disconnected => "Disconnected".into(),
            ConnectionStatus::Authorizing => "Connecting to Microsoft 365".into(),
            ConnectionStatus::Connected(identity) => {
                let account = identity.email.as_deref().unwrap_or(&identity.display_name);
                format!("Connected: {account}")
            }
            ConnectionStatus::ReauthenticationRequired => "Microsoft 365 sign-in required".into(),
            ConnectionStatus::Unavailable(detail) => format!("Unavailable: {detail}"),
        }
    }

    pub(crate) fn take_notice(&mut self) -> Option<String> {
        self.pending_notice.take()
    }

    pub(crate) fn receipt_summary(receipt: &CommandReceipt) -> &'static str {
        match receipt.disposition {
            CommandDisposition::StagedLocally => "Staged in the local preview",
            CommandDisposition::QueuedForSync => "Queued for synchronization",
            CommandDisposition::AppliedRemotely => "Saved to the connected workspace",
        }
    }

    pub(crate) fn reset(&mut self) {
        self.disconnect_microsoft365();
    }
}

#[cfg(test)]
mod tests;
