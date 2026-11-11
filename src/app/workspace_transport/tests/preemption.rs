use super::super::WorkspaceTransportController;
use super::super::remote_worker::{
    InterestKind, RemotePoll, RemotePollPriority, RemoteWork, TransportRuntime,
};
use super::{remote_graph_target, remote_message};
use crate::integrations::{
    AccountIdentity, CommandDisposition, CommandReceipt, ConnectionStatus, DelegatedTokens,
    SyncInterest, TransportError, WorkspaceCommand, WorkspaceDelta, WorkspaceTransport,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

/// Records the order of polls and dispatches. Its first `Chats` poll blocks
/// (a stand-in for the bootstrap fan-out) until it is cancelled or a short
/// deadline passes, so preemption can be observed deterministically.
struct PreemptionTransport {
    events: Arc<Mutex<Vec<String>>>,
    block_first_chats: bool,
    chats_polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

impl WorkspaceTransport for PreemptionTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Connected(AccountIdentity {
            account_id: "remote-account".into(),
            display_name: "Remote Account".into(),
            email: None,
            tenant_id: None,
        })
    }

    fn poll(&mut self, _interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        panic!("remote workers must use the cancellable poll interface")
    }

    fn poll_cancellable(
        &mut self,
        interest: SyncInterest,
        cancellation: &crate::integrations::TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        self.events
            .lock()
            .expect("test event list must not be poisoned")
            .push(format!("poll:{:?}", InterestKind::from(&interest)));
        if interest == SyncInterest::Chats
            && self.chats_polls.fetch_add(1, Ordering::Relaxed) == 0
            && self.block_first_chats
        {
            let _ = self.started.send(());
            let deadline = Instant::now() + Duration::from_millis(400);
            while !cancellation.is_cancelled() && Instant::now() < deadline {
                thread::yield_now();
            }
            if cancellation.is_cancelled() {
                self.cancellation_observed.store(true, Ordering::Relaxed);
                return Err(TransportError::Cancelled);
            }
        }
        Ok(WorkspaceDelta {
            completed: vec![interest],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        self.events
            .lock()
            .expect("test event list must not be poisoned")
            .push("dispatch".into());
        Ok(CommandReceipt {
            command_id: "preemption:test".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        })
    }
}

/// Controller, ordered event log, cancellation flag, and the "blocking poll
/// started" signal.
type PreemptionFixture = (
    WorkspaceTransportController,
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicBool>,
    mpsc::Receiver<()>,
);

fn preemption_controller(block_first_chats: bool) -> PreemptionFixture {
    let events = Arc::new(Mutex::new(Vec::new()));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started, started_receiver) = mpsc::channel();
    let controller =
        WorkspaceTransportController::with_remote_transport(Box::new(PreemptionTransport {
            events: Arc::clone(&events),
            block_first_chats,
            chats_polls: Arc::new(AtomicUsize::new(0)),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started,
        }));
    (controller, events, cancellation_observed, started_receiver)
}

fn started_interest(controller: &WorkspaceTransportController) -> Option<SyncInterest> {
    match &controller.runtime {
        TransportRuntime::RemoteBusy {
            work: RemoteWork::Poll(active),
            ..
        } => Some(active.as_interest().clone()),
        _ => None,
    }
}

fn wait_for_idle(controller: &mut WorkspaceTransportController) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while controller.is_polling() && Instant::now() < deadline {
        controller.collect_remote_work();
        thread::yield_now();
    }
    assert!(!controller.is_polling(), "the worker must finish in time");
}

#[test]
fn poll_priority_orders_identity_selected_then_directory_fifo() {
    let (mut controller, events, _, _) = preemption_controller(false);
    let chat = remote_graph_target("remote-priority-chat");
    controller.queue_remote_interest(SyncInterest::Teams);
    controller.queue_remote_interest(SyncInterest::Calendar);
    controller.request_remote_chat_messages(chat.clone());
    controller
        .pending_remote_interests
        .push_back(RemotePoll::interest(SyncInterest::Identity));

    let mut started = Vec::new();
    for _ in 0..4 {
        controller
            .start_next_remote_poll()
            .expect("a poll must start");
        started.push(started_interest(&controller).expect("a poll must be active"));
        wait_for_idle(&mut controller);
    }
    assert_eq!(
        started,
        vec![
            SyncInterest::Identity,
            SyncInterest::ChatMessages { chat },
            SyncInterest::Teams,
            SyncInterest::Calendar,
        ]
    );
    assert_eq!(
        RemotePoll::interest(SyncInterest::Teams).priority(),
        RemotePollPriority::Directory
    );
    drop(events);
}

#[test]
fn directory_polls_yield_to_a_due_remote_command_without_dropping() {
    let (mut controller, events, _, _) = preemption_controller(false);
    controller.queue_remote_interest(SyncInterest::Teams);
    controller.queue_remote_interest(SyncInterest::Calendar);
    controller.enqueue([remote_message("hello")]);

    // The bootstrap read does not start ahead of the send...
    controller
        .start_next_remote_poll()
        .expect("gated start is not an error");
    assert!(!controller.is_polling());
    assert_eq!(controller.pending_remote_interests.len(), 2);

    // ...and the send dispatches first, with the reads running afterwards.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let _ = controller.dispatch_due();
        if controller.pending_command_count() == 0
            && controller.pending_remote_interests.is_empty()
            && !controller.is_polling()
        {
            break;
        }
        thread::yield_now();
    }
    let events = events.lock().expect("test event list must not be poisoned");
    assert_eq!(events.first().map(String::as_str), Some("dispatch"));
    assert!(events.iter().any(|event| event == "poll:Teams"));
    assert!(events.iter().any(|event| event == "poll:Calendar"));
}

#[test]
fn selected_reads_are_not_gated_by_a_due_command() {
    let (mut controller, _events, _, _) = preemption_controller(false);
    let chat = remote_graph_target("remote-selected-with-command");
    controller.queue_remote_interest(SyncInterest::Teams);
    controller.request_remote_chat_messages(chat.clone());
    controller.enqueue([remote_message("hello")]);
    controller
        .start_next_remote_poll()
        .expect("a selected read must start");
    assert_eq!(
        started_interest(&controller),
        Some(SyncInterest::ChatMessages { chat })
    );
    wait_for_idle(&mut controller);
}

#[test]
fn a_due_command_preempts_a_running_directory_poll_once_and_requeues_it() {
    let (mut controller, events, cancellation_observed, started) = preemption_controller(true);
    controller.queue_remote_interest(SyncInterest::Chats);
    controller
        .start_next_remote_poll()
        .expect("Chats must start");
    started
        .recv_timeout(Duration::from_secs(1))
        .expect("the blocking Chats poll must be running");

    controller.enqueue([remote_message("hello")]);
    let _ = controller.dispatch_due();
    assert!(controller.preempting_directory_poll);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let _ = controller.dispatch_due();
        if controller.pending_command_count() == 0 && !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(cancellation_observed.load(Ordering::Relaxed));
    // The cancelled bootstrap result was never surfaced, the send ran, and
    // the poll was requeued and then ran again to completion.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let _ = controller.dispatch_due();
        if controller.pending_remote_interests.is_empty() && !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    let events = events
        .lock()
        .expect("test event list must not be poisoned")
        .clone();
    assert_eq!(events, ["poll:Chats", "dispatch", "poll:Chats"]);
    assert!(
        controller
            .take_deltas()
            .iter()
            .any(|delta| delta.completed.contains(&SyncInterest::Chats))
    );
}

#[test]
fn an_already_preempted_poll_is_never_preempted_again() {
    let (mut controller, _events, cancellation_observed, started) = preemption_controller(true);
    controller
        .start_remote_poll(RemotePoll::interest(SyncInterest::Chats).already_preempted_for_test())
        .expect("Chats must start");
    started
        .recv_timeout(Duration::from_secs(1))
        .expect("the blocking Chats poll must be running");

    controller.enqueue([remote_message("hello")]);
    let _ = controller.dispatch_due();
    assert!(!controller.preempting_directory_poll);
    assert!(matches!(
        &controller.runtime,
        TransportRuntime::RemoteBusy { cancellation, .. } if !cancellation.is_cancelled()
    ));

    // It finishes normally (not cancelled) and the command follows.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let _ = controller.dispatch_due();
        if controller.pending_command_count() == 0 && !controller.is_polling() {
            break;
        }
        thread::yield_now();
    }
    assert!(!cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(controller.pending_command_count(), 0);
}

#[test]
fn a_running_selected_read_is_never_preempted_by_a_command() {
    let (mut controller, _events, _, _) = preemption_controller(false);
    let chat = remote_graph_target("remote-selected-no-preempt");
    controller
        .start_remote_poll(RemotePoll::selected_chat_messages(
            SyncInterest::ChatMessages { chat },
        ))
        .expect("selected read must start");
    controller.enqueue([remote_message("hello")]);
    let _ = controller.dispatch_due();
    assert!(!controller.preempting_directory_poll);
    wait_for_idle(&mut controller);
}

#[test]
fn next_retry_delay_reports_a_deferred_read_so_the_app_repaints() {
    let (mut controller, _events, _, _) = preemption_controller(false);
    assert_eq!(controller.next_retry_delay(), None);
    let _ = controller.handle_completed_poll(
        SyncInterest::Chats,
        Err(TransportError::Throttled {
            retry_after_seconds: Some(30),
        }),
    );
    // Nothing is queued, so nothing is waiting on the cooldown.
    assert_eq!(controller.next_retry_delay(), None);
    controller.queue_remote_chat_refresh();
    let delay = controller
        .next_retry_delay()
        .expect("a deferred Chats read must schedule a wake-up");
    assert!(delay > Duration::from_secs(25) && delay <= Duration::from_secs(30));
    // The deferred read does not start before its deadline.
    controller
        .start_next_remote_poll()
        .expect("deferral is not an error");
    assert!(!controller.is_polling());
    assert_eq!(controller.pending_remote_interests.len(), 1);
}

#[test]
fn workers_wake_the_ui_on_completion_and_the_waker_survives_replacement() {
    let (mut controller, _events, _, _) = preemption_controller(false);
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    controller.set_repaint_waker(Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    }));
    controller.queue_remote_interest(SyncInterest::Teams);
    controller
        .start_next_remote_poll()
        .expect("poll must start");
    wait_for_idle(&mut controller);
    let deadline = Instant::now() + Duration::from_secs(1);
    while wakes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(wakes.load(Ordering::Relaxed), 1);

    // Replacing or losing the adapter keeps the callback installed.
    controller.disconnect_microsoft365();
    assert!(controller.repaint_waker.is_some());
    controller
        .connect_microsoft365(DelegatedTokens::for_test("waker-test-token"))
        .expect("a delegated token must connect");
    assert!(controller.repaint_waker.is_some());
    controller.fallback_to_local_after_remote_loss(
        "test loss",
        TransportError::Unavailable("lost".into()),
    );
    assert!(controller.repaint_waker.is_some());
}

#[test]
fn a_controller_without_a_waker_still_completes_work() {
    let (mut controller, _events, _, _) = preemption_controller(false);
    controller.queue_remote_interest(SyncInterest::Teams);
    controller
        .start_next_remote_poll()
        .expect("poll must start");
    wait_for_idle(&mut controller);
    assert!(controller.repaint_waker.is_none());
}
