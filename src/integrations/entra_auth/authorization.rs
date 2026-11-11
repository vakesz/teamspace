//! Construction of an Entra authorization-code request.
//!
//! The request owns all PKCE correlation material. A caller must bind it into
//! an [`EntraAuthorizationSession`] before using its browser URL, which makes
//! the registered loopback port live before a browser can be opened.

use super::super::cloud::GraphCloud;
use super::callback::{AuthorizationCallback, LoopbackCallbackError, parse_loopback_callback};
use super::config::EntraPublicClientConfig;
use super::listener::LoopbackCallbackListener;
use super::pkce::PkceSession;
use super::token::{RefreshContext, exchange_callback};
use std::fmt;
use std::time::Duration;
use url::Url;

pub(crate) use super::listener::{AuthorizationCancellation, LoopbackListenerError};
pub(crate) use super::token::{DelegatedTokens, TokenExchangeError};
pub(in crate::integrations) use super::token::{RefreshError, TokenRefresher};

/// One in-memory authorization request. It owns the PKCE verifier and must
/// remain alive until a matching callback is exchanged or cancelled.
pub(crate) struct EntraAuthorizationRequest {
    config: EntraPublicClientConfig,
    pkce: PkceSession,
}

impl EntraAuthorizationRequest {
    /// Starts a fresh in-memory authorization attempt. The request must be
    /// bound with [`Self::bind_loopback`] before a browser URL is available.
    pub(crate) fn begin(
        config: EntraPublicClientConfig,
    ) -> Result<Self, AuthorizationPreparationError> {
        let pkce = PkceSession::generate()
            .map_err(|_| AuthorizationPreparationError::EntropyUnavailable)?;
        Ok(Self::new(config, pkce))
    }

    pub(super) fn new(config: EntraPublicClientConfig, pkce: PkceSession) -> Self {
        Self { config, pkce }
    }

    /// Builds the v2 Entra authorize endpoint with authorization-code, S256
    /// PKCE, exact loopback redirect, and state parameters. No `nonce` is sent because
    /// no `id_token` is requested or consumed.
    ///
    /// This stays private so application code cannot obtain a browser URL
    /// until a matching listener has been bound in an authorization session.
    fn build_authorization_url(&self) -> Result<Url, AuthorizationUrlError> {
        let mut url = Url::parse(&self.config.authorization_endpoint())
            .map_err(|_| AuthorizationUrlError::InvalidConfiguredEndpoint)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("client_id", self.config.client_id());
            query.append_pair("response_type", "code");
            query.append_pair("redirect_uri", &self.config.redirect_uri());
            query.append_pair("response_mode", "query");
            query.append_pair("scope", &self.config.scope_string());
            query.append_pair("state", self.pkce.state());
            query.append_pair("code_challenge", self.pkce.code_challenge());
            query.append_pair("code_challenge_method", "S256");
        }
        Ok(url)
    }

    /// Parses only a callback bound to this request's private state and exact
    /// registered loopback port. A future listener should call this rather
    /// than handling correlation values in the app layer.
    pub(super) fn parse_callback(
        &self,
        request: &[u8],
    ) -> Result<AuthorizationCallback, LoopbackCallbackError> {
        parse_loopback_callback(request, self.pkce.state(), self.config.loopback_port())
    }

    pub(super) const fn loopback_port(&self) -> u16 {
        self.config.loopback_port()
    }

    pub(super) const fn cloud(&self) -> GraphCloud {
        self.config.cloud()
    }

    pub(super) fn token_endpoint(&self) -> String {
        self.config.token_endpoint()
    }

    pub(super) fn client_id(&self) -> &str {
        self.config.client_id()
    }

    /// The non-secret facts a later silent refresh needs. The scope string
    /// is the one this request authorized, so a refresh asks for the same set.
    pub(super) fn refresh_context(&self) -> RefreshContext {
        RefreshContext::new(
            self.config.token_endpoint(),
            self.config.client_id().into(),
            self.config.scope_string(),
        )
    }

    pub(super) fn redirect_uri(&self) -> String {
        self.config.redirect_uri()
    }

    /// Intended solely for the later in-module authorization-code exchange.
    /// Do not include this verifier in the browser URL, logs, UI, or profile
    /// storage.
    pub(super) fn code_verifier_for_exchange(&self) -> &str {
        self.pkce.code_verifier_for_exchange()
    }

    /// Binds the exact configured IPv4 loopback callback before exposing the
    /// authorization URL through a session. No browser is opened here.
    pub(crate) fn bind_loopback(self) -> Result<EntraAuthorizationSession, AuthorizationFlowError> {
        let listener =
            LoopbackCallbackListener::bind(&self).map_err(AuthorizationFlowError::Loopback)?;
        Ok(EntraAuthorizationSession {
            request: self,
            listener,
            cancellation: AuthorizationCancellation::default(),
        })
    }
}

/// A bound, one-shot Entra authorization flow.
///
/// The caller may open [`Self::authorization_url`] only after this value has
/// been created. [`Self::complete_blocking`] is deliberately blocking and is
/// intended for an integration worker, never a UI event loop. Dropping the
/// session closes the bound listener and zeroizes the request's PKCE material.
pub(crate) struct EntraAuthorizationSession {
    request: EntraAuthorizationRequest,
    listener: LoopbackCallbackListener,
    cancellation: AuthorizationCancellation,
}

impl EntraAuthorizationSession {
    /// Returns the URL only after the matching loopback listener is bound.
    pub(crate) fn authorization_url(&self) -> Result<Url, AuthorizationUrlError> {
        self.request.build_authorization_url()
    }

    /// Returns a cancellation handle that contains no authorization secret.
    pub(crate) fn cancellation(&self) -> AuthorizationCancellation {
        self.cancellation.clone()
    }

    /// Waits for one matching callback and exchanges its code for in-memory
    /// delegated tokens. The session is consumed so its listener and PKCE
    /// verifier cannot be reused.
    pub(crate) fn complete_blocking(
        self,
        timeout: Duration,
    ) -> Result<DelegatedTokens, AuthorizationFlowError> {
        let Self {
            request,
            listener,
            cancellation,
        } = self;
        let callback = listener
            .wait_for_callback(&request, timeout, &cancellation)
            .map_err(AuthorizationFlowError::Loopback);
        // The redirect is spent: release the registered loopback port before
        // the (slow, networked) token exchange so nothing else can connect to
        // it and a new sign-in can bind it immediately.
        drop(listener);
        exchange_callback(&request, callback?).map_err(AuthorizationFlowError::Token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorizationPreparationError {
    EntropyUnavailable,
}

impl fmt::Display for AuthorizationPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => {
                formatter.write_str("could not prepare secure Microsoft 365 authorization")
            }
        }
    }
}

impl std::error::Error for AuthorizationPreparationError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorizationUrlError {
    InvalidConfiguredEndpoint,
}

impl fmt::Display for AuthorizationUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguredEndpoint => {
                formatter.write_str("Entra authorization endpoint is invalid")
            }
        }
    }
}

impl std::error::Error for AuthorizationUrlError {}

/// Category-only errors from a bound Entra authorization flow. Diagnostics
/// intentionally never retain callback URLs, authorization codes, PKCE
/// verifiers, access tokens, refresh tokens, or remote error descriptions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorizationFlowError {
    Loopback(LoopbackListenerError),
    Token(TokenExchangeError),
}

impl fmt::Display for AuthorizationFlowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loopback(error) => error.fmt(formatter),
            Self::Token(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AuthorizationFlowError {}

#[cfg(test)]
mod tests {
    use super::{AuthorizationFlowError, EntraAuthorizationRequest, LoopbackListenerError};
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::entra_auth::config::EntraPublicClientConfig;
    use crate::integrations::entra_auth::pkce::PkceSession;
    use std::collections::BTreeMap;
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    use std::time::Duration;

    const CLIENT_ID: &str = "6f2c8b6a-218c-4d3f-b4f4-e8d77d6c9a72";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const STATE: &str = "state-with-at-least-some-safe-entropy";

    fn request() -> EntraAuthorizationRequest {
        request_for_port(8400)
    }

    fn request_for_port(port: u16) -> EntraAuthorizationRequest {
        let port = port.to_string();
        let config = EntraPublicClientConfig::from_values(
            Some(CLIENT_ID),
            Some("organizations"),
            Some(&port),
            Some("offline_access User.Read Chat.Read"),
        )
        .expect("fixture config must parse");
        let pkce = PkceSession::from_values(VERIFIER.into(), STATE.into())
            .expect("fixture PKCE must parse");
        EntraAuthorizationRequest::new(config, pkce)
    }

    fn available_loopback_port() -> u16 {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("test must reserve an ephemeral loopback port");
        listener
            .local_addr()
            .expect("test listener must have an address")
            .port()
    }

    #[test]
    fn authorization_url_host_and_scopes_follow_the_cloud() {
        for cloud in GraphCloud::ALL {
            let config = EntraPublicClientConfig::from_values(
                Some(CLIENT_ID),
                Some("organizations"),
                Some("8400"),
                Some("offline_access User.Read"),
            )
            .expect("fixture config must parse")
            .with_cloud(cloud);
            let pkce = PkceSession::from_values(VERIFIER.into(), STATE.into())
                .expect("fixture PKCE must parse");
            let request = EntraAuthorizationRequest::new(config, pkce);
            let url = request.build_authorization_url().expect("URL");
            assert_eq!(url.host_str(), Some(cloud.login_host()));
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            let expected = if cloud == GraphCloud::Global {
                "offline_access User.Read".to_owned()
            } else {
                format!("offline_access https://{}/User.Read", cloud.graph_host())
            };
            assert_eq!(query.get("scope"), Some(&expected));
        }
    }

    #[test]
    fn url_uses_code_pkce_exact_loopback_and_required_correlation_values() {
        let request = request();
        let url = request
            .build_authorization_url()
            .expect("validated configuration must produce a URL");
        let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(url.path(), "/organizations/oauth2/v2.0/authorize");
        assert_eq!(query.get("client_id"), Some(&CLIENT_ID.into()));
        assert_eq!(query.get("response_type"), Some(&"code".into()));
        assert_eq!(
            query.get("redirect_uri"),
            Some(&"http://127.0.0.1:8400/oauth/callback".into())
        );
        assert_eq!(query.get("response_mode"), Some(&"query".into()));
        assert_eq!(
            query.get("scope"),
            Some(&"offline_access User.Read Chat.Read".into())
        );
        assert_eq!(query.get("state"), Some(&STATE.into()));
        assert_eq!(query.get("nonce"), None);
        assert_eq!(
            query.get("code_challenge"),
            Some(&"E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into())
        );
        assert_eq!(query.get("code_challenge_method"), Some(&"S256".into()));
        assert!(!url.as_str().contains(VERIFIER));
        assert_eq!(request.pkce.state(), STATE);
        assert_eq!(request.config.loopback_port(), 8400);
        assert_eq!(request.code_verifier_for_exchange(), VERIFIER);

        let callback = request
            .parse_callback(
                format!(
                    "GET /oauth/callback?code=abc&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("request-owned callback state must validate");
        assert_eq!(callback.authorization_code(), Some("abc"));
    }

    #[test]
    fn beginning_a_request_keeps_pkce_correlation_values_out_of_the_app_api() {
        let config = EntraPublicClientConfig::from_values(
            Some(CLIENT_ID),
            Some("organizations"),
            Some("8400"),
            Some("offline_access User.Read"),
        )
        .expect("fixture config must parse");
        let request = EntraAuthorizationRequest::begin(config)
            .expect("OS entropy must be available in tests");
        let url = request
            .build_authorization_url()
            .expect("request must expose a browser URL");

        assert!(url.query_pairs().any(|(name, _)| name == "state"));
        assert!(url.query_pairs().all(|(name, _)| name != "nonce"));
        assert!(url.query_pairs().any(|(name, _)| name == "code_challenge"));
    }

    #[test]
    fn bound_session_exposes_the_browser_url_only_after_its_loopback_port_is_live() {
        let port = available_loopback_port();
        let session = request_for_port(port)
            .bind_loopback()
            .expect("an available loopback port must bind");
        let url = session
            .authorization_url()
            .expect("bound session must build an authorization URL");
        assert_eq!(
            url.query_pairs()
                .find(|(name, _)| name == "redirect_uri")
                .map(|(_, value)| value.into_owned()),
            Some(format!("http://127.0.0.1:{port}/oauth/callback"))
        );
        assert!(TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).is_err());

        let cancellation = session.cancellation();
        cancellation.cancel();
        assert!(matches!(
            session.complete_blocking(Duration::from_secs(1)),
            Err(AuthorizationFlowError::Loopback(
                LoopbackListenerError::Cancelled
            ))
        ));
    }
}
