use super::{CommandDispatch, WorkspaceTransportController};
use crate::integrations::{
    AccountIdentity, CommandDisposition, CommandReceipt, ConnectionStatus, ProviderResourceRef,
    RemoteCalendarProjection, RemoteChatConversation, RemoteChatKind, RemoteChatMessageImportance,
    RemoteChatMessagesProjection, RemoteChatProjection, RemoteTeamsProjection, SyncInterest,
    TransportError, WorkspaceCommand, WorkspaceDelta, WorkspaceTransport,
};
use crate::shared::reaction::ReactionKind;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

mod cancellation;
mod dispatch_ambiguous;
mod dispatch_refresh;
mod preemption;
mod profile_lifecycle;
mod refresh;
mod retry_and_loss;
mod sync_basics;
mod throttle;

struct FailingTransport {
    polls: Arc<AtomicUsize>,
}

impl WorkspaceTransport for FailingTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Disconnected
    }

    fn poll(&mut self, _interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(TransportError::Unavailable("offline for this test".into()))
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unavailable("offline for this test".into()))
    }
}

struct BackgroundTransport {
    polls: Arc<AtomicUsize>,
}

struct SequencedRemoteTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
}

/// A remote adapter that waits for the controller's cooperative signal on
/// its first poll, then returns a deliberately stale identity result. It
/// proves both that the controller invokes the cancellable interface and
/// that a late result cannot be associated with the next profile.
struct CancellationAwareTransport {
    polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

/// Blocks only a selected-channel read. It verifies the controller can
/// replace an obsolete selection without invalidating the authenticated
/// profile or cancelling unrelated bootstrap work.
struct ChannelSelectionCancellationTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    channel_message_polls: Arc<AtomicUsize>,
    channel_files_polls: Arc<AtomicUsize>,
    channel_reply_polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

/// Blocks one delivery-triggered reply refresh long enough for a user to
/// select another root. Unlike the selected-read fixture above, the first
/// reply poll is expected to complete without cancellation.
struct ReplyRefreshIsolationTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    block_first_reply: bool,
}

/// Blocks the first selected-chat history poll so lifecycle tests can prove a
/// replacement cancels only that target-bound read, not the global Chats
/// projection queued alongside it.
struct ChatHistorySelectionCancellationTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    chat_message_polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

/// Blocks the first selected-chat pinned-message read so lifecycle tests can
/// prove a replacement cancels only the contextual pins panel read—not the
/// main history, global directory, or another selected surface.
struct PinnedChatSelectionCancellationTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    pinned_message_polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

/// Blocks the first selected OneDrive file-preview read so lifecycle tests
/// can prove a new preview target cancels only that preview work.
struct OneDriveFilePreviewSelectionCancellationTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    preview_polls: Arc<AtomicUsize>,
    cancellation_observed: Arc<AtomicBool>,
    started: mpsc::Sender<()>,
}

struct RecordingTransport {
    dispatches: Arc<AtomicUsize>,
}

struct ScriptedTransport {
    outcomes: VecDeque<Result<(), TransportError>>,
    attempted: Arc<Mutex<Vec<WorkspaceCommand>>>,
    sequence: u64,
}

struct RemoteDispatchTransport {
    polls: Arc<Mutex<Vec<SyncInterest>>>,
    dispatch_threads: Arc<Mutex<Vec<thread::ThreadId>>>,
    outcomes: VecDeque<Result<CommandReceipt, TransportError>>,
}

struct BlockingRemoteDispatchTransport {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl WorkspaceTransport for RecordingTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::LocalPreview
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        Ok(WorkspaceDelta {
            completed: vec![interest],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        let sequence = self
            .dispatches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        Ok(CommandReceipt {
            command_id: format!("recording:{sequence}"),
            disposition: CommandDisposition::StagedLocally,
            upload: None,
        })
    }
}

impl WorkspaceTransport for ScriptedTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Disconnected
    }

    fn poll(&mut self, _interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        Ok(WorkspaceDelta::default())
    }

    fn dispatch(&mut self, command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        self.attempted
            .lock()
            .expect("test attempt list must not be poisoned")
            .push(command);
        match self.outcomes.pop_front().unwrap_or(Ok(())) {
            Ok(()) => {
                self.sequence += 1;
                Ok(CommandReceipt {
                    command_id: format!("scripted:{}", self.sequence),
                    disposition: CommandDisposition::StagedLocally,
                    upload: None,
                })
            }
            Err(error) => Err(error),
        }
    }
}

impl WorkspaceTransport for RemoteDispatchTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Connected(AccountIdentity {
            account_id: "remote-account".into(),
            display_name: "Remote Account".into(),
            email: None,
            tenant_id: None,
        })
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        let mut delta = WorkspaceDelta {
            completed: vec![interest.clone()],
            ..WorkspaceDelta::default()
        };
        if interest == SyncInterest::Chats {
            delta.remote_chats = Some(RemoteChatProjection::default());
        }
        if interest
            == (SyncInterest::ChatMessages {
                chat: remote_chat_target(),
            })
        {
            delta.remote_chat_messages = Some(RemoteChatMessagesProjection::for_test(
                "19:remote-chat@thread.v2",
                Vec::new(),
            ));
        }
        Ok(delta)
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        self.dispatch_threads
            .lock()
            .expect("test dispatch thread list must not be poisoned")
            .push(thread::current().id());
        self.outcomes.pop_front().unwrap_or_else(|| {
            Ok(CommandReceipt {
                command_id: "remote:test".into(),
                disposition: CommandDisposition::AppliedRemotely,
                upload: None,
            })
        })
    }
}

impl WorkspaceTransport for BlockingRemoteDispatchTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Connected(AccountIdentity {
            account_id: "remote-account".into(),
            display_name: "Remote Account".into(),
            email: None,
            tenant_id: None,
        })
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        Ok(WorkspaceDelta {
            completed: vec![interest],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        self.started
            .send(())
            .expect("the test must wait for the remote dispatch");
        self.release
            .recv_timeout(Duration::from_secs(1))
            .expect("the test must release the remote dispatch");
        Ok(CommandReceipt {
            command_id: "remote:blocking".into(),
            disposition: CommandDisposition::AppliedRemotely,
            upload: None,
        })
    }
}

impl WorkspaceTransport for BackgroundTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Authorizing
    }

    fn poll(&mut self, _interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        self.polls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(WorkspaceDelta {
            completed: vec![SyncInterest::Identity],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for SequencedRemoteTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Connected(AccountIdentity {
            account_id: "remote-account".into(),
            display_name: "Remote Account".into(),
            email: Some("remote@example.test".into()),
            tenant_id: None,
        })
    }

    fn poll(&mut self, interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        match interest {
            SyncInterest::Identity => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Identity],
                identity: Some(AccountIdentity {
                    account_id: "remote-account".into(),
                    display_name: "Remote Account".into(),
                    email: Some("remote@example.test".into()),
                    tenant_id: None,
                }),
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                identity: None,
                remote_chats: Some(RemoteChatProjection::default()),
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Teams => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: Some(RemoteTeamsProjection::default()),
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: None,
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Calendar => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Calendar],
                identity: None,
                remote_chats: None,
                remote_chat_messages: None,
                remote_pinned_chat_messages: None,
                remote_chat_members: None,
                remote_message_search: None,
                remote_teams: None,
                remote_channel_messages: None,
                remote_channel_files: None,
                remote_channel_replies: None,
                remote_calendar: Some(RemoteCalendarProjection::default()),
                remote_files: None,
                remote_file_preview: None,
                remote_capabilities: None,
                notices: Vec::new(),
            }),
            SyncInterest::Files => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Files],
                ..WorkspaceDelta::default()
            }),
            SyncInterest::ChannelMessages { team, channel } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelMessages { team, channel }],
                ..WorkspaceDelta::default()
            }),
            SyncInterest::ChannelFiles { team, channel } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelFiles { team, channel }],
                ..WorkspaceDelta::default()
            }),
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            } => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::ChannelReplies {
                    team,
                    channel,
                    post,
                }],
                ..WorkspaceDelta::default()
            }),
            _ => Err(TransportError::Unsupported(
                "not configured by this test transport".into(),
            )),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for CancellationAwareTransport {
    fn connection_status(&self) -> ConnectionStatus {
        ConnectionStatus::Authorizing
    }

    fn poll(&mut self, _interest: SyncInterest) -> Result<WorkspaceDelta, TransportError> {
        panic!("remote workers must use the cancellable poll interface")
    }

    fn poll_cancellable(
        &mut self,
        interest: SyncInterest,
        cancellation: &crate::integrations::TransportCancellation,
    ) -> Result<WorkspaceDelta, TransportError> {
        let poll_number = self.polls.fetch_add(1, Ordering::Relaxed);
        if poll_number == 0 {
            self.started
                .send(())
                .expect("the test must still wait for the first worker");
            let deadline = Instant::now() + Duration::from_secs(1);
            while !cancellation.is_cancelled() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            self.cancellation_observed
                .store(cancellation.is_cancelled(), Ordering::Relaxed);
            return Ok(identity_delta("stale-account"));
        }

        match interest {
            SyncInterest::Identity => Ok(identity_delta("fresh-account")),
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                remote_chats: Some(RemoteChatProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Teams => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                remote_teams: Some(RemoteTeamsProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Calendar => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Calendar],
                remote_calendar: Some(RemoteCalendarProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Files => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Files],
                ..WorkspaceDelta::default()
            }),
            _ => Err(TransportError::Unsupported(
                "not configured by this test transport".into(),
            )),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for ChannelSelectionCancellationTransport {
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
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        match interest {
            SyncInterest::Identity => Ok(identity_delta("remote-account")),
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                remote_chats: Some(RemoteChatProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Teams => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Teams],
                remote_teams: Some(RemoteTeamsProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Calendar => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Calendar],
                remote_calendar: Some(RemoteCalendarProjection::default()),
                ..WorkspaceDelta::default()
            }),
            SyncInterest::Files => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Files],
                ..WorkspaceDelta::default()
            }),
            SyncInterest::ChannelMessages { team, channel } => {
                if self.channel_message_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for the selected channel read");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected channel read was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::ChannelMessages { team, channel }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            SyncInterest::ChannelFiles { team, channel } => {
                if self.channel_files_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for the selected channel-files read");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected channel-files read was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::ChannelFiles { team, channel }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            } => {
                if self.channel_reply_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for the selected reply read");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected reply read was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::ChannelReplies {
                            team,
                            channel,
                            post,
                        }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            _ => Err(TransportError::Unsupported(
                "not configured by this test transport".into(),
            )),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for ReplyRefreshIsolationTransport {
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
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        let SyncInterest::ChannelReplies {
            team,
            channel,
            post,
        } = interest
        else {
            return Err(TransportError::Unsupported(
                "this test transport supports only channel-reply reads".into(),
            ));
        };

        if self.block_first_reply {
            self.block_first_reply = false;
            self.started
                .send(())
                .expect("the test must wait for the delivery refresh");
            self.release
                .recv_timeout(Duration::from_secs(1))
                .expect("the test must release the delivery refresh");
            self.cancellation_observed
                .store(cancellation.is_cancelled(), Ordering::Relaxed);
            if cancellation.is_cancelled() {
                return Err(TransportError::Cancelled);
            }
        }

        Ok(WorkspaceDelta {
            completed: vec![SyncInterest::ChannelReplies {
                team,
                channel,
                post,
            }],
            ..WorkspaceDelta::default()
        })
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for ChatHistorySelectionCancellationTransport {
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
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        match interest {
            SyncInterest::ChatMessages { chat } => {
                if self.chat_message_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for the selected chat history");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected chat history was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::ChatMessages { chat }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                remote_chats: Some(RemoteChatProjection::default()),
                ..WorkspaceDelta::default()
            }),
            other => Ok(WorkspaceDelta {
                completed: vec![other],
                ..WorkspaceDelta::default()
            }),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for PinnedChatSelectionCancellationTransport {
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
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        match interest {
            SyncInterest::ChatPinnedMessages { chat } => {
                if self.pinned_message_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for selected pinned messages");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected pinned-message read was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::ChatPinnedMessages { chat }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            SyncInterest::Chats => Ok(WorkspaceDelta {
                completed: vec![SyncInterest::Chats],
                remote_chats: Some(RemoteChatProjection::default()),
                ..WorkspaceDelta::default()
            }),
            other => Ok(WorkspaceDelta {
                completed: vec![other],
                ..WorkspaceDelta::default()
            }),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

impl WorkspaceTransport for OneDriveFilePreviewSelectionCancellationTransport {
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
        self.polls
            .lock()
            .expect("test poll list must not be poisoned")
            .push(interest.clone());
        match interest {
            SyncInterest::OneDriveFilePreview { item } => {
                if self.preview_polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    self.started
                        .send(())
                        .expect("the test must wait for the selected file preview");
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while !cancellation.is_cancelled() && Instant::now() < deadline {
                        thread::yield_now();
                    }
                    let cancelled = cancellation.is_cancelled();
                    self.cancellation_observed
                        .store(cancelled, Ordering::Relaxed);
                    if cancelled {
                        Err(TransportError::Cancelled)
                    } else {
                        Err(TransportError::Unavailable(
                            "the selected OneDrive preview was not cancelled by the test".into(),
                        ))
                    }
                } else {
                    Ok(WorkspaceDelta {
                        completed: vec![SyncInterest::OneDriveFilePreview { item }],
                        ..WorkspaceDelta::default()
                    })
                }
            }
            other => Ok(WorkspaceDelta {
                completed: vec![other],
                ..WorkspaceDelta::default()
            }),
        }
    }

    fn dispatch(&mut self, _command: WorkspaceCommand) -> Result<CommandReceipt, TransportError> {
        Err(TransportError::Unsupported(
            "not needed in this test".into(),
        ))
    }
}

fn identity_delta(account_id: &str) -> WorkspaceDelta {
    WorkspaceDelta {
        completed: vec![SyncInterest::Identity],
        identity: Some(AccountIdentity {
            account_id: account_id.into(),
            display_name: account_id.into(),
            email: None,
            tenant_id: None,
        }),
        ..WorkspaceDelta::default()
    }
}

fn remote_graph_target(identifier: &str) -> ProviderResourceRef {
    RemoteChatConversation::for_test(
        identifier,
        RemoteChatKind::OneOnOne,
        "Remote chat",
        "Microsoft 365",
        "RC",
        false,
        Vec::new(),
    )
    .target()
}

fn remote_chat_target() -> ProviderResourceRef {
    remote_graph_target("19:remote-chat@thread.v2")
}

fn remote_message(body: &str) -> WorkspaceCommand {
    WorkspaceCommand::SendChatMessage {
        chat: remote_chat_target(),
        body: body.into(),
        importance: RemoteChatMessageImportance::Normal,
        attachment: None,
    }
}

fn remote_message_to(chat: ProviderResourceRef, body: &str) -> WorkspaceCommand {
    WorkspaceCommand::SendChatMessage {
        chat,
        body: body.into(),
        importance: RemoteChatMessageImportance::Urgent,
        attachment: None,
    }
}

fn remote_quote_reply(
    chat: ProviderResourceRef,
    message: ProviderResourceRef,
    body: &str,
) -> WorkspaceCommand {
    WorkspaceCommand::ReplyToChatMessageWithQuote {
        chat,
        messages: vec![message],
        body: body.into(),
    }
}

fn remote_chat_message_edit(
    chat: ProviderResourceRef,
    message: ProviderResourceRef,
    body: &str,
) -> WorkspaceCommand {
    WorkspaceCommand::EditChatMessage {
        chat,
        message,
        body: body.into(),
    }
}

fn remote_chat_message_delete(
    chat: ProviderResourceRef,
    message: ProviderResourceRef,
) -> WorkspaceCommand {
    WorkspaceCommand::DeleteChatMessage { chat, message }
}

fn remote_chat_message_reaction(
    chat: ProviderResourceRef,
    message: ProviderResourceRef,
    selected: bool,
) -> WorkspaceCommand {
    WorkspaceCommand::SetChatMessageReaction {
        chat,
        message,
        reaction: ReactionKind::Like,
        selected,
    }
}

fn remote_channel_post(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    body: &str,
) -> WorkspaceCommand {
    WorkspaceCommand::SendChannelPost {
        team,
        channel,
        body: body.into(),
    }
}

fn remote_channel_reply(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    body: &str,
) -> WorkspaceCommand {
    WorkspaceCommand::ReplyToChannelPost {
        team,
        channel,
        post,
        body: body.into(),
    }
}

fn remote_channel_post_reaction(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    selected: bool,
) -> WorkspaceCommand {
    WorkspaceCommand::SetChannelMessageReaction {
        team,
        channel,
        post,
        reaction: ReactionKind::Like,
        selected,
    }
}

fn remote_channel_reply_reaction(
    team: ProviderResourceRef,
    channel: ProviderResourceRef,
    post: ProviderResourceRef,
    reply: ProviderResourceRef,
    selected: bool,
) -> WorkspaceCommand {
    WorkspaceCommand::SetChannelMessageReplyReaction {
        team,
        channel,
        post,
        reply,
        reaction: ReactionKind::Like,
        selected,
    }
}

fn wait_for_dispatches(controller: &mut WorkspaceTransportController) -> Vec<CommandDispatch> {
    for _ in 0..20_000 {
        let outcomes = controller.dispatch_due();
        if !outcomes.is_empty() {
            return outcomes;
        }
        thread::yield_now();
    }
    panic!("the remote worker did not return a dispatch outcome");
}
