use super::super::{CommandDispatch, WorkspaceTransportController};
use super::{
    CancellationAwareTransport, RemoteDispatchTransport, remote_message, wait_for_dispatches,
};
use crate::integrations::{
    CommandDisposition, DelegatedTokens, ProviderResourceRef, RemoteChatMessageImportance,
    SyncInterest, WorkspaceCommand,
};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

#[test]
fn profile_transition_cancels_old_work_discards_its_delta_and_restarts_identity() {
    let polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(CancellationAwareTransport {
            polls: Arc::clone(&polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        }));

    controller
        .synchronize_profile("profile-a")
        .expect("the initial remote worker must start");
    assert!(controller.take_remote_projection_clear_requested());
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );

    controller
        .synchronize_profile("profile-b")
        .expect("changing profiles must never surface the old worker result");
    assert!(controller.take_remote_projection_clear_requested());

    for _ in 0..20_000 {
        controller
            .synchronize_profile("profile-b")
            .expect("the fresh profile bootstrap must succeed");
        if !controller.is_polling() && polls.load(Ordering::Relaxed) == 6 {
            break;
        }
        std::thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(polls.load(Ordering::Relaxed), 6);
    let deltas = controller.take_deltas();
    assert_eq!(deltas.len(), 5);
    assert!(deltas.iter().all(|delta| {
        delta
            .identity
            .as_ref()
            .is_none_or(|identity| identity.account_id == "fresh-account")
    }));
    assert!(deltas.iter().any(|delta| {
        delta
            .identity
            .as_ref()
            .is_some_and(|identity| identity.account_id == "fresh-account")
    }));
    assert!(
        deltas
            .iter()
            .any(|delta| delta.completed == [SyncInterest::Chats])
    );
    assert!(
        deltas
            .iter()
            .any(|delta| delta.completed == [SyncInterest::Teams])
    );
    assert!(
        deltas
            .iter()
            .any(|delta| delta.completed == [SyncInterest::Calendar])
    );
    assert!(
        deltas
            .iter()
            .any(|delta| delta.completed == [SyncInterest::Files])
    );
}

#[test]
fn disconnect_cancels_an_in_flight_remote_worker_and_requests_projection_clear() {
    let polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(CancellationAwareTransport {
            polls: Arc::clone(&polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        }));

    controller
        .synchronize_profile("profile")
        .expect("the remote worker must start");
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );
    let _ = controller.take_remote_projection_clear_requested();

    controller.disconnect_microsoft365();

    for _ in 0..10_000 {
        if cancellation_observed.load(Ordering::Relaxed) {
            break;
        }
        std::thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert!(!controller.is_microsoft365_active());
    assert!(controller.take_remote_projection_clear_requested());
}

#[test]
fn connecting_a_new_account_cancels_an_in_flight_remote_worker() {
    let polls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let (started_sender, started_receiver) = mpsc::channel();
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(CancellationAwareTransport {
            polls: Arc::clone(&polls),
            cancellation_observed: Arc::clone(&cancellation_observed),
            started: started_sender,
        }));

    controller
        .synchronize_profile("profile")
        .expect("the remote worker must start");
    assert!(
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
    );
    let _ = controller.take_remote_projection_clear_requested();

    controller
        .connect_microsoft365(DelegatedTokens::for_test("replacement-account-token"))
        .expect("a valid replacement token must construct before the old worker is cancelled");

    for _ in 0..10_000 {
        if cancellation_observed.load(Ordering::Relaxed) {
            break;
        }
        std::thread::yield_now();
    }

    assert!(cancellation_observed.load(Ordering::Relaxed));
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert!(controller.is_microsoft365_active());
    assert!(controller.take_remote_projection_clear_requested());
}

#[test]
fn remote_dispatch_runs_on_a_worker_and_returns_its_intent_id() {
    let polls = Arc::new(Mutex::new(Vec::new()));
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::clone(&polls),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::new(),
        }));
    let expected_id = controller.enqueue([remote_message("Send from a worker")])[0];
    let caller_thread = thread::current().id();

    assert!(controller.dispatch_due().is_empty());
    let outcomes = wait_for_dispatches(&mut controller);

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::Delivered {
            intent_id,
            receipt,
        }] if *intent_id == expected_id
            && receipt.disposition == CommandDisposition::AppliedRemotely
    ));
    let dispatch_threads = dispatch_threads
        .lock()
        .expect("test dispatch thread list must not be poisoned");
    assert_eq!(dispatch_threads.len(), 1);
    assert_ne!(dispatch_threads[0], caller_thread);
    drop(dispatch_threads);
    for _ in 0..10_000 {
        if polls
            .lock()
            .expect("test poll list must not be poisoned")
            .contains(&SyncInterest::Chats)
        {
            break;
        }
        thread::yield_now();
    }
    assert!(
        polls
            .lock()
            .expect("test poll list must not be poisoned")
            .contains(&SyncInterest::Chats)
    );
}

#[test]
fn local_targets_bypass_remote_transport_even_when_microsoft365_is_active() {
    let dispatch_threads = Arc::new(Mutex::new(Vec::new()));
    let mut controller =
        WorkspaceTransportController::with_remote_transport(Box::new(RemoteDispatchTransport {
            polls: Arc::new(Mutex::new(Vec::new())),
            dispatch_threads: Arc::clone(&dispatch_threads),
            outcomes: VecDeque::new(),
        }));
    let expected_id = controller.enqueue([WorkspaceCommand::SendChatMessage {
        chat: ProviderResourceRef::local_preview("local-chat"),
        body: "This must remain a local preview message".into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }])[0];

    let outcomes = controller.dispatch_due();

    assert!(matches!(
        outcomes.as_slice(),
        [CommandDispatch::Delivered {
            intent_id,
            receipt,
        }] if *intent_id == expected_id
            && receipt.disposition == CommandDisposition::StagedLocally
    ));
    assert!(
        dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .is_empty()
    );
}
