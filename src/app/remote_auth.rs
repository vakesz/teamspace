//! App-side ownership of an explicit Microsoft 365 browser sign-in attempt.
//!
//! This module deliberately owns only the short-lived interactive lifecycle:
//! it reads tenant-owned public-client configuration, binds the verified
//! loopback listener through `entra_auth`, opens the system browser only after
//! that bind succeeds, and returns in-memory credentials to the workspace
//! transport. It never stores, renders, logs, or exposes the credentials.

use crate::app::browser::open_system_browser;
use crate::integrations::cloud::GraphCloud;
use crate::integrations::entra_auth::authorization::{
    AuthorizationCancellation, AuthorizationFlowError, DelegatedTokens, EntraAuthorizationRequest,
    LoopbackListenerError, TokenExchangeError,
};
use crate::integrations::entra_auth::config::EntraPublicClientConfig;
use std::fmt;
use std::io;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;
use url::Url;

const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Coordinates at most one browser authorization attempt at a time.
///
/// The worker owns the bound loopback session and all PKCE material. The UI
/// retains only a cancellation handle and a result receiver, so it can never
/// accidentally inspect a code verifier, authorization code, or token.
pub(crate) struct RemoteAuthorizationController {
    state: AuthorizationState,
    cloud: GraphCloud,
}

enum AuthorizationState {
    Idle,
    Waiting {
        receiver: Receiver<Result<DelegatedTokens, RemoteAuthorizationFailure>>,
        cancellation: AuthorizationCancellation,
    },
}

impl Default for RemoteAuthorizationController {
    fn default() -> Self {
        Self {
            state: AuthorizationState::Idle,
            // An invalid value falls back to the global label for display
            // only; starting sign-in re-reads it and reports the error.
            cloud: GraphCloud::from_environment().unwrap_or_default(),
        }
    }
}

impl RemoteAuthorizationController {
    /// Label of the cloud selected by `TEAMSPACE_CLOUD`, for the connect menu.
    pub(crate) const fn cloud_label(&self) -> &'static str {
        self.cloud.label()
    }

    /// Starts a tenant-configured authorization-code + PKCE flow.
    ///
    /// The only browser launch happens here, after the listener is bound and
    /// the request-owned redirect URL has been built. A missing or malformed
    /// configuration never opens a browser.
    pub(crate) fn start(&mut self) -> Result<(), RemoteAuthorizationStartError> {
        let config = EntraPublicClientConfig::from_environment()
            // Causes dropped on purpose: configuration errors may echo env values.
            .map_err(|_| RemoteAuthorizationStartError::InvalidConfiguration)?
            .ok_or(RemoteAuthorizationStartError::NotConfigured)?;
        self.start_with(config, |url| {
            // Causes dropped on purpose: the launch error may embed the authorization URL.
            open_system_browser(url).map_err(|_| RemoteAuthorizationStartError::BrowserUnavailable)
        })
    }

    fn start_with(
        &mut self,
        config: EntraPublicClientConfig,
        launch_browser: impl FnOnce(&Url) -> Result<(), RemoteAuthorizationStartError>,
    ) -> Result<(), RemoteAuthorizationStartError> {
        if self.is_authorizing() {
            return Err(RemoteAuthorizationStartError::AlreadyInProgress);
        }

        let port = config.loopback_port();
        // Causes dropped on purpose: PKCE/state setup errors must never be
        // formatted, so only a category is surfaced.
        let request = EntraAuthorizationRequest::begin(config)
            .map_err(|_| RemoteAuthorizationStartError::PreparationFailed)?;
        // Safe exception: a bind failure carries only an `io::ErrorKind` for a
        // fixed loopback port, so keep it to let users diagnose conflicts.
        let session = request.bind_loopback().map_err(|error| {
            let kind = match error {
                AuthorizationFlowError::Loopback(LoopbackListenerError::BindFailed(kind)) => {
                    Some(kind)
                }
                _ => None,
            };
            RemoteAuthorizationStartError::LoopbackUnavailable { port, kind }
        })?;
        // Causes dropped on purpose: URL-building errors may involve request secrets.
        let authorization_url = session
            .authorization_url()
            .map_err(|_| RemoteAuthorizationStartError::InvalidConfiguration)?;

        // Do not spawn the blocking receiver until the browser launch has
        // succeeded. On launch failure `session` drops here, immediately
        // closing the listener and zeroizing the request-owned PKCE values.
        launch_browser(&authorization_url)?;

        let cancellation = session.cancellation();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("teamspace-entra-sign-in".into())
            .spawn(move || {
                let outcome = session
                    .complete_blocking(AUTHORIZATION_TIMEOUT)
                    .map_err(RemoteAuthorizationFailure::from_flow_error);
                let _ = sender.send(outcome);
            })
            .map_err(|_| RemoteAuthorizationStartError::WorkerUnavailable)?;

        self.state = AuthorizationState::Waiting {
            receiver,
            cancellation,
        };
        Ok(())
    }

    /// Requests cancellation and drops the UI-side result receiver. The
    /// worker sees the shared cancellation signal, closes its listener, and
    /// drops any token it may have received before the cancellation won.
    pub(crate) fn cancel(&mut self) {
        if let AuthorizationState::Waiting { cancellation, .. } = &self.state {
            cancellation.cancel();
        }
        self.state = AuthorizationState::Idle;
    }

    /// Takes one completed in-memory credential handoff or a category-only
    /// outcome. A `DelegatedTokens` value is intentionally move-only and can
    /// be passed straight to the integration-owned transport bridge.
    pub(crate) fn poll(&mut self) -> Option<Result<DelegatedTokens, RemoteAuthorizationFailure>> {
        let state = std::mem::replace(&mut self.state, AuthorizationState::Idle);
        let AuthorizationState::Waiting {
            receiver,
            cancellation,
        } = state
        else {
            return None;
        };

        match receiver.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => {
                self.state = AuthorizationState::Waiting {
                    receiver,
                    cancellation,
                };
                None
            }
            Err(TryRecvError::Disconnected) => Some(Err(RemoteAuthorizationFailure::WorkerStopped)),
        }
    }

    pub(crate) const fn is_authorizing(&self) -> bool {
        matches!(&self.state, AuthorizationState::Waiting { .. })
    }
}

/// Safe, UI-facing failures before a browser authorization worker starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteAuthorizationStartError {
    AlreadyInProgress,
    NotConfigured,
    InvalidConfiguration,
    PreparationFailed,
    LoopbackUnavailable {
        port: u16,
        kind: Option<io::ErrorKind>,
    },
    BrowserUnavailable,
    WorkerUnavailable,
}

impl fmt::Display for RemoteAuthorizationStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInProgress => {
                formatter.write_str("Microsoft 365 sign-in is already in progress")
            }
            Self::NotConfigured => {
                formatter.write_str("Microsoft 365 sign-in is not configured for this tenant")
            }
            Self::InvalidConfiguration => {
                formatter.write_str("Microsoft 365 sign-in configuration is incomplete or invalid")
            }
            Self::PreparationFailed => {
                formatter.write_str("could not prepare secure Microsoft 365 sign-in")
            }
            Self::LoopbackUnavailable { port, kind } => match kind {
                Some(io::ErrorKind::AddrInUse) => write!(
                    formatter,
                    "port {port} is already in use; close the program using it and try again"
                ),
                Some(io::ErrorKind::PermissionDenied) => write!(
                    formatter,
                    "not permitted to listen on port {port} for the Microsoft 365 sign-in callback"
                ),
                _ => write!(
                    formatter,
                    "could not reserve the registered Microsoft 365 sign-in callback on port {port}"
                ),
            },
            Self::BrowserUnavailable => {
                formatter.write_str("could not open the system browser for Microsoft 365 sign-in")
            }
            Self::WorkerUnavailable => {
                formatter.write_str("could not start the Microsoft 365 sign-in worker")
            }
        }
    }
}

/// Safe, UI-facing failures from a running authorization worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteAuthorizationFailure {
    Cancelled,
    TimedOut,
    Rejected,
    Unavailable,
    WorkerStopped,
}

impl RemoteAuthorizationFailure {
    fn from_flow_error(error: AuthorizationFlowError) -> Self {
        match error {
            AuthorizationFlowError::Loopback(LoopbackListenerError::Cancelled) => Self::Cancelled,
            AuthorizationFlowError::Loopback(LoopbackListenerError::TimedOut) => Self::TimedOut,
            AuthorizationFlowError::Token(
                TokenExchangeError::AuthorizationDenied
                | TokenExchangeError::Unauthorized
                | TokenExchangeError::Rejected,
            ) => Self::Rejected,
            AuthorizationFlowError::Loopback(_) | AuthorizationFlowError::Token(_) => {
                Self::Unavailable
            }
        }
    }
}

impl fmt::Display for RemoteAuthorizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Microsoft 365 sign-in was cancelled"),
            Self::TimedOut => formatter.write_str("Microsoft 365 sign-in timed out"),
            Self::Rejected => formatter.write_str("Microsoft 365 sign-in was rejected"),
            Self::Unavailable => formatter.write_str("Microsoft 365 sign-in could not complete"),
            Self::WorkerStopped => {
                formatter.write_str("Microsoft 365 sign-in stopped before it could complete")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteAuthorizationStartError;
    use super::{
        AuthorizationCancellation, AuthorizationState, RemoteAuthorizationController,
        RemoteAuthorizationFailure,
    };
    use crate::integrations::cloud::GraphCloud;
    use std::io;
    use std::sync::mpsc;

    #[test]
    fn loopback_errors_explain_port_conflicts_without_other_detail() {
        let message = |kind| {
            RemoteAuthorizationStartError::LoopbackUnavailable {
                port: 8400,
                kind: Some(kind),
            }
            .to_string()
        };

        assert_eq!(
            message(io::ErrorKind::AddrInUse),
            "port 8400 is already in use; close the program using it and try again"
        );
        assert_eq!(
            message(io::ErrorKind::PermissionDenied),
            "not permitted to listen on port 8400 for the Microsoft 365 sign-in callback"
        );
        assert_eq!(
            RemoteAuthorizationStartError::LoopbackUnavailable {
                port: 8400,
                kind: None
            }
            .to_string(),
            "could not reserve the registered Microsoft 365 sign-in callback on port 8400"
        );
    }

    #[test]
    fn idle_controller_has_no_pending_result() {
        let mut controller = RemoteAuthorizationController::default();

        assert!(!controller.is_authorizing());
        assert!(controller.poll().is_none());
    }

    #[test]
    fn completed_failure_is_taken_once_and_returns_to_idle() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Err(RemoteAuthorizationFailure::TimedOut))
            .expect("test receiver must be connected");
        let mut controller = RemoteAuthorizationController {
            state: AuthorizationState::Waiting {
                receiver,
                cancellation: AuthorizationCancellation::default(),
            },
            cloud: GraphCloud::Global,
        };

        assert!(matches!(
            controller.poll(),
            Some(Err(RemoteAuthorizationFailure::TimedOut))
        ));
        assert!(!controller.is_authorizing());
        assert!(controller.poll().is_none());
    }

    #[test]
    fn cancellation_drops_the_receiver_so_late_results_are_discarded() {
        let (sender, receiver) = mpsc::channel();
        let mut controller = RemoteAuthorizationController {
            state: AuthorizationState::Waiting {
                receiver,
                cancellation: AuthorizationCancellation::default(),
            },
            cloud: GraphCloud::Global,
        };

        controller.cancel();

        assert!(!controller.is_authorizing());
        assert!(controller.poll().is_none());
        assert!(
            sender
                .send(Err(RemoteAuthorizationFailure::Rejected))
                .is_err()
        );
    }
}
