//! Authorization-code exchange with an Entra v2 token endpoint.
//!
//! This module exchanges one request-owned code and PKCE verifier into
//! in-memory delegated tokens. It does not persist, log, render, clone, or
//! serialize credentials. A refresh token is kept only inside an in-memory
//! [`TokenRefresher`] that the Graph adapter's worker owns for the session; it
//! is never persisted, logged, or rendered.

use super::super::cloud::GraphCloud;
use super::authorization::EntraAuthorizationRequest;
use super::callback::AuthorizationCallback;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, HeaderMap, RETRY_AFTER};
use reqwest::redirect::Policy;
use serde::Deserialize;
use std::fmt;
use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_TOKEN_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;
const MAX_REFRESH_TOKEN_BYTES: usize = 32 * 1024;
const MAX_SCOPE_BYTES: usize = 2 * 1024;
const MAX_EXPIRES_IN_SECONDS: u64 = 31 * 24 * 60 * 60;

/// In-memory delegated credentials returned by a successful code exchange.
///
/// This type intentionally does not implement `Debug`, `Clone`, `Serialize`,
/// or `Deserialize`. Only sibling integration modules can access its secret
/// values when constructing an authenticated provider transport.
pub(crate) struct DelegatedTokens {
    access_token: SecretValue,
    /// Moved into `refresher` by the exchange; never read from outside.
    refresh_token: Option<SecretValue>,
    /// When the access token lapses, computed when the response was parsed.
    expires_at: Instant,
    /// The cloud that issued these tokens. It stays bound to the tokens so
    /// they can only ever be presented to that cloud's Graph host.
    cloud: GraphCloud,
    refresher: Option<TokenRefresher>,
    /// The scope string Entra reported for this token, when it sent one.
    granted_scope: Option<String>,
}

/// The credentials an integration-owned transport needs, moved out of
/// [`DelegatedTokens`] in one step.
pub(in crate::integrations) struct Microsoft365Credentials {
    pub(in crate::integrations) access_token: String,
    pub(in crate::integrations) expires_at: Instant,
    pub(in crate::integrations) refresher: Option<TokenRefresher>,
    pub(in crate::integrations) cloud: GraphCloud,
    pub(in crate::integrations) granted_scope: Option<String>,
}

impl DelegatedTokens {
    /// Moves the credentials directly into an integration-owned transport.
    ///
    /// The raw string never crosses the integrations boundary: this method is
    /// intentionally unavailable to the app and feature modules. Moving the
    /// existing allocation avoids retaining a second unzeroized token copy
    /// during the handoff.
    pub(in crate::integrations) fn into_microsoft365_credentials(self) -> Microsoft365Credentials {
        Microsoft365Credentials {
            access_token: self.access_token.into_inner(),
            expires_at: self.expires_at,
            refresher: self.refresher,
            cloud: self.cloud,
            granted_scope: self.granted_scope,
        }
    }
}

#[cfg(test)]
impl DelegatedTokens {
    pub(crate) fn for_test(access_token: impl Into<String>) -> Self {
        Self {
            access_token: SecretValue::new(access_token.into()),
            refresh_token: None,
            expires_at: Instant::now() + Duration::from_secs(3600),
            cloud: GraphCloud::Global,
            refresher: None,
            granted_scope: None,
        }
    }

    pub(crate) fn for_test_in(cloud: GraphCloud, access_token: impl Into<String>) -> Self {
        Self {
            cloud,
            ..Self::for_test(access_token)
        }
    }
}

/// Non-secret facts a silent refresh needs, fixed when the code was exchanged.
pub(in crate::integrations) struct RefreshContext {
    token_endpoint: String,
    client_id: String,
    scope: String,
}

impl RefreshContext {
    pub(super) const fn new(token_endpoint: String, client_id: String, scope: String) -> Self {
        Self {
            token_endpoint,
            client_id,
            scope,
        }
    }
}

/// Category-only failure of a silent refresh. It carries no server text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::integrations) enum RefreshError {
    /// Entra refused the grant or sent an unusable response. The refresh
    /// token must not be presented again.
    Rejected,
    Throttled {
        retry_after_seconds: Option<u64>,
    },
    /// Network, server, or oversize failure; the refresh token stays usable.
    Unavailable,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rejected => "Entra rejected the session renewal",
            Self::Throttled { .. } => "Entra token endpoint is throttled",
            Self::Unavailable => "Entra token endpoint is unavailable",
        })
    }
}

impl std::error::Error for RefreshError {}

/// A new access token and its expiry from one successful refresh.
pub(in crate::integrations) struct RefreshedAccess {
    access_token: SecretValue,
    expires_at: Instant,
    /// The scope Entra reported, when the response carried one.
    granted_scope: Option<String>,
}

impl RefreshedAccess {
    pub(in crate::integrations) fn into_parts(self) -> (String, Instant, Option<String>) {
        (
            self.access_token.into_inner(),
            self.expires_at,
            self.granted_scope,
        )
    }
}

/// Holds one session's refresh token in memory and renews the access token.
///
/// Intentionally neither `Debug` nor `Clone`. The refresh token zeroizes when
/// this value drops, including when the session drops it after a rejection.
pub(in crate::integrations) struct TokenRefresher {
    context: RefreshContext,
    refresh_token: SecretValue,
    http: Box<dyn TokenHttpClient>,
}

impl TokenRefresher {
    /// Posts one `refresh_token` grant. A rotated refresh token replaces the
    /// stored one only after the whole response validates; a response without
    /// one keeps the existing token.
    pub(in crate::integrations) fn refresh(&mut self) -> Result<RefreshedAccess, RefreshError> {
        let fields = [
            ("client_id", self.context.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", self.refresh_token.expose()),
            ("scope", self.context.scope.as_str()),
        ];
        let response = self
            .http
            .post_form(&self.context.token_endpoint, &fields)
            .map_err(|_| RefreshError::Unavailable)?;
        match response.status {
            200 => {}
            429 => {
                return Err(RefreshError::Throttled {
                    retry_after_seconds: response.retry_after_seconds,
                });
            }
            status if (500..=599).contains(&status) => return Err(RefreshError::Unavailable),
            _ => return Err(RefreshError::Rejected),
        }
        let mut tokens =
            parse_delegated_tokens(&response.body).map_err(|_| RefreshError::Rejected)?;
        if let Some(rotated) = tokens.refresh_token.take() {
            self.refresh_token = rotated;
        }
        Ok(RefreshedAccess {
            access_token: tokens.access_token,
            expires_at: tokens.expires_at,
            granted_scope: tokens.granted_scope,
        })
    }
}

struct SecretValue(String);

impl SecretValue {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }

    fn into_inner(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Category-only failures from a token exchange. No variant carries a code,
/// verifier, access token, refresh token, or server-provided description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenExchangeError {
    AuthorizationDenied,
    ClientUnavailable,
    Network,
    ResponseTooLarge,
    Unauthorized,
    Rejected,
    Throttled { retry_after_seconds: Option<u64> },
    ServerUnavailable,
    InvalidTokenResponse,
}

impl fmt::Display for TokenExchangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthorizationDenied => formatter.write_str("Entra authorization was denied"),
            Self::ClientUnavailable => {
                formatter.write_str("could not create the Entra token client")
            }
            Self::Network => formatter.write_str("could not reach the Entra token endpoint"),
            Self::ResponseTooLarge => {
                formatter.write_str("Entra token endpoint returned an unexpectedly large response")
            }
            Self::Unauthorized => formatter.write_str("Entra token endpoint rejected the client"),
            Self::Rejected => {
                formatter.write_str("Entra token endpoint rejected the authorization")
            }
            Self::Throttled {
                retry_after_seconds: Some(seconds),
            } => write!(
                formatter,
                "Entra token endpoint is throttled; retry after {seconds}s"
            ),
            Self::Throttled {
                retry_after_seconds: None,
            } => formatter.write_str("Entra token endpoint is throttled"),
            Self::ServerUnavailable => {
                formatter.write_str("Entra token endpoint is temporarily unavailable")
            }
            Self::InvalidTokenResponse => {
                formatter.write_str("Entra token endpoint returned an invalid token response")
            }
        }
    }
}

impl std::error::Error for TokenExchangeError {}

pub(super) fn exchange_callback(
    request: &EntraAuthorizationRequest,
    callback: AuthorizationCallback,
) -> Result<DelegatedTokens, TokenExchangeError> {
    let client = ReqwestTokenHttpClient::new()?;
    // The refresher outlives this exchange, but builds its own client only
    // if a renewal is ever needed.
    exchange_callback_with_client(
        request,
        callback,
        &client,
        Box::new(LazyReqwestTokenHttpClient::default()),
    )
}

fn exchange_callback_with_client(
    request: &EntraAuthorizationRequest,
    callback: AuthorizationCallback,
    client: &dyn TokenHttpClient,
    refresh_http: Box<dyn TokenHttpClient>,
) -> Result<DelegatedTokens, TokenExchangeError> {
    let Some(code) = callback.authorization_code() else {
        return Err(TokenExchangeError::AuthorizationDenied);
    };
    let endpoint = request.token_endpoint();
    let redirect_uri = request.redirect_uri();
    let fields = [
        ("client_id", request.client_id()),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", request.code_verifier_for_exchange()),
    ];
    let response = client
        .post_form(&endpoint, &fields)
        .map_err(TokenHttpError::into_exchange_error)?;
    match response.status {
        200 => parse_delegated_tokens(&response.body).map(|mut tokens| {
            tokens.cloud = request.cloud();
            tokens.refresher = tokens
                .refresh_token
                .take()
                .map(|refresh_token| TokenRefresher {
                    context: request.refresh_context(),
                    refresh_token,
                    http: refresh_http,
                });
            tokens
        }),
        400 => Err(TokenExchangeError::Rejected),
        401 | 403 => Err(TokenExchangeError::Unauthorized),
        429 => Err(TokenExchangeError::Throttled {
            retry_after_seconds: response.retry_after_seconds,
        }),
        status if (500..=599).contains(&status) => Err(TokenExchangeError::ServerUnavailable),
        _ => Err(TokenExchangeError::Rejected),
    }
}

struct ReqwestTokenHttpClient {
    client: Client,
}

impl ReqwestTokenHttpClient {
    fn new() -> Result<Self, TokenExchangeError> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .user_agent("teamspace/0.1")
            .build()
            .map_err(|_| TokenExchangeError::ClientUnavailable)?;
        Ok(Self { client })
    }
}

impl TokenHttpClient for ReqwestTokenHttpClient {
    fn post_form(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
    ) -> Result<TokenHttpResponse, TokenHttpError> {
        let response = self
            .client
            .post(endpoint)
            .header(ACCEPT, "application/json")
            .form(fields)
            .send()
            .map_err(|_| TokenHttpError::Network)?;
        let status = response.status().as_u16();
        let retry_after_seconds = retry_after_seconds(response.headers());
        // The body can contain access and refresh tokens. Keep it zeroizing
        // before it is promoted into the response wrapper so early network or
        // oversize exits cannot free raw credential bytes untouched.
        let mut body = Zeroizing::new(Vec::new());
        response
            .take(MAX_TOKEN_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| TokenHttpError::Network)?;
        if body.len() as u64 > MAX_TOKEN_RESPONSE_BYTES {
            return Err(TokenHttpError::ResponseTooLarge);
        }
        Ok(TokenHttpResponse {
            status,
            retry_after_seconds,
            body: std::mem::take(&mut *body),
        })
    }
}

/// Builds the blocking client on first use: construction starts a runtime
/// thread and loads TLS roots, and most sessions never need a renewal.
#[derive(Default)]
struct LazyReqwestTokenHttpClient {
    client: OnceLock<Option<ReqwestTokenHttpClient>>,
}

impl TokenHttpClient for LazyReqwestTokenHttpClient {
    fn post_form(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
    ) -> Result<TokenHttpResponse, TokenHttpError> {
        self.client
            .get_or_init(|| ReqwestTokenHttpClient::new().ok())
            .as_ref()
            .ok_or(TokenHttpError::Network)?
            .post_form(endpoint, fields)
    }
}

fn retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

trait TokenHttpClient: Send + Sync {
    fn post_form(
        &self,
        endpoint: &str,
        fields: &[(&str, &str)],
    ) -> Result<TokenHttpResponse, TokenHttpError>;
}

struct TokenHttpResponse {
    status: u16,
    retry_after_seconds: Option<u64>,
    body: Vec<u8>,
}

impl Drop for TokenHttpResponse {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

enum TokenHttpError {
    Network,
    ResponseTooLarge,
}

impl TokenHttpError {
    fn into_exchange_error(self) -> TokenExchangeError {
        match self {
            Self::Network => TokenExchangeError::Network,
            Self::ResponseTooLarge => TokenExchangeError::ResponseTooLarge,
        }
    }
}

#[derive(Deserialize)]
struct TokenResponsePayload {
    token_type: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

impl Drop for TokenResponsePayload {
    fn drop(&mut self) {
        self.token_type.zeroize();
        self.access_token.zeroize();
        if let Some(refresh_token) = &mut self.refresh_token {
            refresh_token.zeroize();
        }
        if let Some(scope) = &mut self.scope {
            scope.zeroize();
        }
    }
}

fn parse_delegated_tokens(body: &[u8]) -> Result<DelegatedTokens, TokenExchangeError> {
    let mut payload = serde_json::from_slice::<TokenResponsePayload>(body)
        .map_err(|_| TokenExchangeError::InvalidTokenResponse)?;
    if !payload.token_type.eq_ignore_ascii_case("bearer")
        || !is_valid_token(&payload.access_token, MAX_ACCESS_TOKEN_BYTES)
        || payload
            .refresh_token
            .as_ref()
            .is_some_and(|token| !is_valid_token(token, MAX_REFRESH_TOKEN_BYTES))
        || payload.expires_in == 0
        || payload.expires_in > MAX_EXPIRES_IN_SECONDS
        || payload
            .scope
            .as_ref()
            .is_some_and(|scope| !is_valid_scope(scope))
    {
        return Err(TokenExchangeError::InvalidTokenResponse);
    }
    let access_token = SecretValue::new(std::mem::take(&mut payload.access_token));
    let refresh_token = payload.refresh_token.take().map(SecretValue::new);
    Ok(DelegatedTokens {
        access_token,
        refresh_token,
        expires_at: Instant::now() + Duration::from_secs(payload.expires_in),
        cloud: GraphCloud::Global,
        refresher: None,
        granted_scope: payload.scope.take(),
    })
}

fn is_valid_token(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value == value.trim()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
}

fn is_valid_scope(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SCOPE_BYTES && !value.chars().any(char::is_control)
}

/// A scripted token endpoint for transport-level tests in sibling modules.
#[cfg(test)]
pub(in crate::integrations) mod test_support {
    use super::{
        GraphCloud, RefreshContext, SecretValue, TokenHttpClient, TokenHttpError,
        TokenHttpResponse, TokenRefresher,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    pub(in crate::integrations) enum ScriptedRefresh {
        Response { status: u16, body: String },
        Network,
        TooLarge,
    }

    impl ScriptedRefresh {
        pub(in crate::integrations) fn tokens(body: &str) -> Self {
            Self::Response {
                status: 200,
                body: body.into(),
            }
        }

        pub(in crate::integrations) fn status(status: u16) -> Self {
            Self::Response {
                status,
                body: "{}".into(),
            }
        }
    }

    /// Every `(endpoint, form fields)` the refresher posted.
    pub(in crate::integrations) type RecordedRefreshes =
        Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

    struct ScriptedClient {
        responses: Mutex<VecDeque<ScriptedRefresh>>,
        requests: RecordedRefreshes,
    }

    impl TokenHttpClient for ScriptedClient {
        fn post_form(
            &self,
            endpoint: &str,
            fields: &[(&str, &str)],
        ) -> Result<TokenHttpResponse, TokenHttpError> {
            self.requests.lock().expect("test lock").push((
                endpoint.into(),
                fields
                    .iter()
                    .map(|(name, value)| ((*name).into(), (*value).into()))
                    .collect(),
            ));
            match self
                .responses
                .lock()
                .expect("test lock")
                .pop_front()
                .expect("an unscripted token request was made")
            {
                ScriptedRefresh::Response { status, body } => Ok(TokenHttpResponse {
                    status,
                    retry_after_seconds: (status == 429).then_some(7),
                    body: body.into_bytes(),
                }),
                ScriptedRefresh::Network => Err(TokenHttpError::Network),
                ScriptedRefresh::TooLarge => Err(TokenHttpError::ResponseTooLarge),
            }
        }
    }

    pub(in crate::integrations) fn refresher(
        cloud: GraphCloud,
        refresh_token: &str,
        script: impl IntoIterator<Item = ScriptedRefresh>,
    ) -> (TokenRefresher, RecordedRefreshes) {
        let requests = RecordedRefreshes::default();
        let refresher = TokenRefresher {
            context: RefreshContext::new(
                format!(
                    "https://{}/organizations/oauth2/v2.0/token",
                    cloud.login_host()
                ),
                "client-id".into(),
                "User.Read offline_access".into(),
            ),
            refresh_token: SecretValue::new(refresh_token.into()),
            http: Box::new(ScriptedClient {
                responses: Mutex::new(script.into_iter().collect()),
                requests: Arc::clone(&requests),
            }),
        };
        (refresher, requests)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_delegated_tokens;
    use super::test_support::{ScriptedRefresh, refresher};
    use super::{
        RefreshError, TokenExchangeError, TokenHttpClient, TokenHttpError, TokenHttpResponse,
        exchange_callback_with_client as exchange_with_refresh_client,
    };
    use crate::integrations::cloud::GraphCloud;
    use crate::integrations::entra_auth::authorization::EntraAuthorizationRequest;
    use crate::integrations::entra_auth::callback::AuthorizationCallback;
    use crate::integrations::entra_auth::config::EntraPublicClientConfig;
    use crate::integrations::entra_auth::pkce::PkceSession;
    use crate::integrations::{ConnectionStatus, Microsoft365Transport, WorkspaceTransport};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const CLIENT_ID: &str = "6f2c8b6a-218c-4d3f-b4f4-e8d77d6c9a72";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const STATE: &str = "state-with-at-least-some-safe-entropy";

    type RecordedFormFields = Vec<(String, String)>;
    type RecordedRequest = (String, RecordedFormFields);

    struct TestHttpClient {
        responses: Mutex<VecDeque<Result<TokenHttpResponse, TokenHttpError>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl TestHttpClient {
        fn with_response(response: Result<TokenHttpResponse, TokenHttpError>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from([response])),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl TokenHttpClient for TestHttpClient {
        fn post_form(
            &self,
            endpoint: &str,
            fields: &[(&str, &str)],
        ) -> Result<TokenHttpResponse, TokenHttpError> {
            self.requests.lock().expect("test lock must work").push((
                endpoint.into(),
                fields
                    .iter()
                    .map(|(name, value)| ((*name).into(), (*value).into()))
                    .collect(),
            ));
            self.responses
                .lock()
                .expect("test lock must work")
                .pop_front()
                .expect("test response must exist")
        }
    }

    impl TokenHttpClient for Arc<TestHttpClient> {
        fn post_form(
            &self,
            endpoint: &str,
            fields: &[(&str, &str)],
        ) -> Result<TokenHttpResponse, TokenHttpError> {
            (**self).post_form(endpoint, fields)
        }
    }

    /// Exchanges with a refresh endpoint that must never be reached.
    fn exchange_callback_with_client(
        request: &EntraAuthorizationRequest,
        callback: AuthorizationCallback,
        client: &dyn TokenHttpClient,
    ) -> Result<super::DelegatedTokens, TokenExchangeError> {
        exchange_with_refresh_client(
            request,
            callback,
            client,
            Box::new(TestHttpClient {
                responses: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
            }),
        )
    }

    fn request() -> EntraAuthorizationRequest {
        let config = EntraPublicClientConfig::from_values(
            Some(CLIENT_ID),
            Some("organizations"),
            Some("8400"),
            Some("offline_access User.Read Chat.Read"),
        )
        .expect("test config must parse");
        EntraAuthorizationRequest::new(
            config,
            PkceSession::from_values(VERIFIER.into(), STATE.into()).expect("test PKCE must parse"),
        )
    }

    fn callback(code: &str) -> AuthorizationCallback {
        request()
            .parse_callback(
                format!(
                    "GET /oauth/callback?code={code}&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("test callback must parse")
    }

    fn response(status: u16, body: &str) -> TokenHttpResponse {
        TokenHttpResponse {
            status,
            retry_after_seconds: None,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn exchange_posts_to_each_clouds_token_endpoint_and_binds_the_cloud_to_the_tokens() {
        for cloud in GraphCloud::ALL {
            let request = EntraAuthorizationRequest::new(
                EntraPublicClientConfig::from_values(
                    Some(CLIENT_ID),
                    Some("organizations"),
                    Some("8400"),
                    Some("offline_access User.Read"),
                )
                .expect("test config must parse")
                .with_cloud(cloud),
                PkceSession::from_values(VERIFIER.into(), STATE.into())
                    .expect("test PKCE must parse"),
            );
            let client = TestHttpClient::with_response(Ok(response(
                200,
                r#"{"token_type":"Bearer","access_token":"access-token","expires_in":3600}"#,
            )));
            let tokens =
                exchange_callback_with_client(&request, callback("authorization-code"), &client)
                    .expect("valid token response must exchange");
            let credentials = tokens.into_microsoft365_credentials();
            assert_eq!(credentials.cloud, cloud);
            let requests = client.requests.lock().expect("test lock must work");
            assert_eq!(
                requests[0].0,
                format!(
                    "https://{}/organizations/oauth2/v2.0/token",
                    cloud.login_host()
                )
            );
        }
    }

    #[test]
    fn authorization_code_exchange_uses_exact_pkce_v2_form_and_keeps_tokens_in_memory() {
        let request = request();
        let client = TestHttpClient::with_response(Ok(response(
            200,
            r#"{"token_type":"Bearer","access_token":"access-token","refresh_token":"refresh-token","expires_in":3600,"scope":"User.Read Chat.Read"}"#,
        )));

        let tokens =
            exchange_callback_with_client(&request, callback("authorization-code"), &client)
                .expect("valid token response must exchange");

        assert!(tokens.refresher.is_some());
        assert!(tokens.refresh_token.is_none());
        let remaining = tokens.expires_at.saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(3590) && remaining <= Duration::from_secs(3600));
        let transport = Microsoft365Transport::from_delegated_tokens(tokens)
            .expect("a validated delegated token must create the Graph transport");
        assert!(matches!(
            transport.connection_status(),
            ConnectionStatus::Authorizing
        ));
        let requests = client.requests.lock().expect("test lock must work");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].0,
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        assert_eq!(
            requests[0].1,
            vec![
                ("client_id".into(), CLIENT_ID.into()),
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), "authorization-code".into()),
                (
                    "redirect_uri".into(),
                    "http://127.0.0.1:8400/oauth/callback".into(),
                ),
                ("code_verifier".into(), VERIFIER.into()),
            ]
        );
    }

    #[test]
    fn denial_network_and_http_errors_are_normalized_without_echoing_secrets() {
        let request = request();
        let denial = request
            .parse_callback(
                format!(
                    "GET /oauth/callback?error=access_denied&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("denial callback must parse");
        let client = TestHttpClient::with_response(Err(TokenHttpError::Network));
        assert!(matches!(
            exchange_callback_with_client(&request, denial, &client),
            Err(TokenExchangeError::AuthorizationDenied)
        ));
        assert!(
            client
                .requests
                .lock()
                .expect("test lock must work")
                .is_empty()
        );

        let client = TestHttpClient::with_response(Err(TokenHttpError::Network));
        let error =
            match exchange_callback_with_client(&request, callback("sensitive-code"), &client) {
                Err(error) => error,
                Ok(_) => panic!("network failure must be normalized"),
            };
        assert_eq!(error, TokenExchangeError::Network);
        assert!(!error.to_string().contains("sensitive-code"));
        assert!(!error.to_string().contains(VERIFIER));

        for (status, expected) in [
            (400, TokenExchangeError::Rejected),
            (401, TokenExchangeError::Unauthorized),
            (403, TokenExchangeError::Unauthorized),
            (500, TokenExchangeError::ServerUnavailable),
        ] {
            let client = TestHttpClient::with_response(Ok(response(status, "{}")));
            assert!(matches!(
                exchange_callback_with_client(&request, callback("authorization-code"), &client),
                Err(error) if error == expected
            ));
        }
    }

    #[test]
    fn throttling_and_malformed_token_payloads_are_rejected_without_accepting_partial_tokens() {
        let request = request();
        let client = TestHttpClient::with_response(Ok(TokenHttpResponse {
            status: 429,
            retry_after_seconds: Some(13),
            body: b"{not-json}".to_vec(),
        }));
        assert!(matches!(
            exchange_callback_with_client(&request, callback("authorization-code"), &client),
            Err(TokenExchangeError::Throttled {
                retry_after_seconds: Some(13)
            })
        ));

        for body in [
            r#"{"token_type":"Bearer","access_token":"","expires_in":3600}"#,
            r#"{"token_type":"NotBearer","access_token":"token","expires_in":3600}"#,
            r#"{"token_type":"Bearer","access_token":"token","expires_in":0}"#,
            r#"{"token_type":"Bearer","access_token":"token\ninvalid","expires_in":3600}"#,
        ] {
            let client = TestHttpClient::with_response(Ok(response(200, body)));
            assert!(matches!(
                exchange_callback_with_client(&request, callback("authorization-code"), &client),
                Err(TokenExchangeError::InvalidTokenResponse)
            ));
        }
    }

    const TOKENS_A: &str = r#"{"token_type":"Bearer","access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"scope":"User.Read"}"#;
    const TOKENS_NO_ROTATION: &str =
        r#"{"token_type":"Bearer","access_token":"access-3","expires_in":1800}"#;

    fn form(
        requests: &super::test_support::RecordedRefreshes,
        index: usize,
    ) -> Vec<(String, String)> {
        requests.lock().expect("test lock")[index].1.clone()
    }

    #[test]
    fn exchange_hands_a_refresher_bound_to_the_requests_scope_and_endpoint() {
        let request = request();
        let exchange = Arc::new(TestHttpClient::with_response(Ok(response(
            200,
            r#"{"token_type":"Bearer","access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}"#,
        ))));
        let refresh_side = Arc::new(TestHttpClient::with_response(Ok(response(200, TOKENS_A))));
        let tokens = exchange_with_refresh_client(
            &request,
            callback("authorization-code"),
            &*exchange,
            Box::new(Arc::clone(&refresh_side)),
        )
        .expect("exchange must succeed");
        let mut refresher = tokens
            .into_microsoft365_credentials()
            .refresher
            .expect("a refresh token must produce a refresher");
        refresher.refresh().expect("refresh must succeed");
        let requests = refresh_side.requests.lock().expect("test lock");
        assert_eq!(
            requests[0].0,
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        assert_eq!(
            requests[0].1,
            vec![
                ("client_id".into(), CLIENT_ID.into()),
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), "refresh-token".into()),
                ("scope".into(), "offline_access User.Read Chat.Read".into()),
            ]
        );
        assert!(exchange.requests.lock().expect("test lock").len() == 1);
    }

    #[test]
    fn refresh_form_and_endpoint_follow_each_cloud() {
        for cloud in GraphCloud::ALL {
            let (mut refresher, requests) =
                refresher(cloud, "refresh-1", [ScriptedRefresh::tokens(TOKENS_A)]);
            refresher.refresh().expect("refresh must succeed");
            let recorded = requests.lock().expect("test lock");
            assert_eq!(
                recorded[0].0,
                format!(
                    "https://{}/organizations/oauth2/v2.0/token",
                    cloud.login_host()
                )
            );
            assert_eq!(
                recorded[0]
                    .1
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                ["client_id", "grant_type", "refresh_token", "scope"]
            );
        }
    }

    #[test]
    fn rotation_replaces_the_refresh_token_and_never_resends_the_old_one() {
        let (mut refresher, requests) = refresher(
            GraphCloud::Global,
            "refresh-1",
            [
                ScriptedRefresh::tokens(TOKENS_A),
                ScriptedRefresh::tokens(TOKENS_NO_ROTATION),
                ScriptedRefresh::tokens(TOKENS_NO_ROTATION),
            ],
        );
        let (access, expires_at, scope) = refresher.refresh().expect("first refresh").into_parts();
        assert_eq!(access, "access-2");
        assert_eq!(scope.as_deref(), Some("User.Read"));
        assert!(expires_at > Instant::now() + Duration::from_secs(3500));
        refresher.refresh().expect("second refresh");
        // A response without a refresh token keeps the rotated one.
        refresher.refresh().expect("third refresh");
        let sent = |i| form(&requests, i)[2].1.clone();
        assert_eq!(sent(0), "refresh-1");
        assert_eq!(sent(1), "refresh-2");
        assert_eq!(sent(2), "refresh-2");
    }

    #[test]
    fn granted_scope_follows_the_token_response_and_is_absent_when_not_sent() {
        let tokens = parse_delegated_tokens(
            br#"{"token_type":"Bearer","access_token":"a","expires_in":60,"scope":"User.Read Chat.ReadWrite"}"#,
        )
        .expect("valid response");
        assert_eq!(
            tokens
                .into_microsoft365_credentials()
                .granted_scope
                .as_deref(),
            Some("User.Read Chat.ReadWrite")
        );
        let tokens = parse_delegated_tokens(
            br#"{"token_type":"Bearer","access_token":"a","expires_in":60}"#,
        )
        .expect("valid response");
        assert!(
            tokens
                .into_microsoft365_credentials()
                .granted_scope
                .is_none()
        );

        let (mut refresher, _requests) = refresher(
            GraphCloud::Global,
            "refresh-1",
            [
                ScriptedRefresh::tokens(
                    r#"{"token_type":"Bearer","access_token":"a2","expires_in":60,"scope":"User.Read Chat.ReadWrite"}"#,
                ),
                ScriptedRefresh::tokens(TOKENS_NO_ROTATION),
            ],
        );
        let (_, _, scope) = refresher.refresh().expect("refresh").into_parts();
        assert_eq!(scope.as_deref(), Some("User.Read Chat.ReadWrite"));
    }

    #[test]
    fn failed_or_invalid_refresh_never_replaces_the_stored_refresh_token() {
        let (mut refresher, requests) = refresher(
            GraphCloud::Global,
            "refresh-1",
            [
                // Valid rotation, but the access token is a non-bearer type.
                ScriptedRefresh::tokens(
                    r#"{"token_type":"mac","access_token":"a","refresh_token":"refresh-evil","expires_in":60}"#,
                ),
                ScriptedRefresh::tokens(TOKENS_A),
            ],
        );
        assert_eq!(refresher.refresh().err(), Some(RefreshError::Rejected));
        refresher.refresh().expect("later refresh");
        assert_eq!(form(&requests, 1)[2].1, "refresh-1");
    }

    #[test]
    fn refresh_failures_map_to_category_errors() {
        for (script, expected) in [
            (ScriptedRefresh::status(400), RefreshError::Rejected),
            (ScriptedRefresh::status(401), RefreshError::Rejected),
            (ScriptedRefresh::status(403), RefreshError::Rejected),
            (
                ScriptedRefresh::status(429),
                RefreshError::Throttled {
                    retry_after_seconds: Some(7),
                },
            ),
            (ScriptedRefresh::status(500), RefreshError::Unavailable),
            (ScriptedRefresh::status(503), RefreshError::Unavailable),
            (ScriptedRefresh::Network, RefreshError::Unavailable),
            (ScriptedRefresh::TooLarge, RefreshError::Unavailable),
        ] {
            let (mut refresher, _) = refresher(GraphCloud::Global, "refresh-1", [script]);
            assert_eq!(refresher.refresh().err(), Some(expected));
        }
    }

    #[test]
    fn invalid_refresh_responses_are_rejected() {
        for body in [
            r#"{"token_type":"NotBearer","access_token":"token","expires_in":3600}"#,
            r#"{"token_type":"Bearer","access_token":"to\nken","expires_in":3600}"#,
            r#"{"token_type":"Bearer","access_token":"token","refresh_token":" padded ","expires_in":3600}"#,
            r#"{"token_type":"Bearer","access_token":"token","expires_in":0}"#,
            "not json",
        ] {
            let (mut refresher, _) = refresher(
                GraphCloud::Global,
                "refresh-1",
                [ScriptedRefresh::tokens(body)],
            );
            assert_eq!(
                refresher.refresh().err(),
                Some(RefreshError::Rejected),
                "{body}"
            );
        }
    }

    #[test]
    fn refresh_errors_never_display_or_debug_secrets() {
        let (mut refresher, _) = refresher(
            GraphCloud::Global,
            "very-secret-refresh",
            [ScriptedRefresh::Response {
                status: 400,
                body: r#"{"error_description":"very-secret-body"}"#.into(),
            }],
        );
        let error = refresher.refresh().err().expect("400 must fail");
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("very-secret"));
        }
    }
}
