//! Silent renewal behavior of `GraphSession`: reads renew and retry once,
//! mutations are never replayed, and a rejected refresh token is dropped.

use super::{
    GraphContentRedirect, GraphCredentials, GraphHttpClient, GraphHttpError, GraphHttpResponse,
    GraphHttpStatus, GraphJsonRequest, GraphSession, Microsoft365Transport, TOKEN_REFRESH_SKEW,
    refresh_due,
};
use crate::integrations::cloud::GraphCloud;
use crate::integrations::entra_auth::authorization::TokenRefresher;
use crate::integrations::entra_auth::test_support::{
    RecordedRefreshes, ScriptedRefresh, refresher,
};
use crate::integrations::{
    ConnectionStatus, ProviderResourceRef, TransportError, WorkspaceCommand, WorkspaceTransport,
};
use crate::shared::reaction::ReactionKind;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

type GraphLog = Arc<Mutex<Vec<(&'static str, String)>>>;

/// A Graph fake that answers each request with the next scripted status and
/// records which token it was sent with.
struct ScriptedGraph {
    statuses: Mutex<VecDeque<u16>>,
    log: GraphLog,
}

impl ScriptedGraph {
    fn next(&self, kind: &'static str, token: &str) -> u16 {
        self.log.lock().expect("log").push((kind, token.into()));
        self.statuses
            .lock()
            .expect("statuses")
            .pop_front()
            .expect("an unscripted Graph request was made")
    }
}

impl GraphHttpClient for ScriptedGraph {
    fn get_me(&self, _url: &Url, token: &str) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(http_response(self.next("get_me", token)))
    }

    fn get_json(
        &self,
        _url: &Url,
        token: &str,
        _max: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(http_response(self.next("get_json", token)))
    }

    fn post_json_query(
        &self,
        _request: GraphJsonRequest<'_>,
        token: &str,
        _max: u64,
    ) -> Result<GraphHttpResponse, GraphHttpError> {
        Ok(http_response(self.next("query", token)))
    }

    fn post_json_status(
        &self,
        _request: GraphJsonRequest<'_>,
        token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status: self.next("post", token),
            retry_after_seconds: None,
        })
    }

    fn patch_json_status(
        &self,
        _request: GraphJsonRequest<'_>,
        token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status: self.next("patch", token),
            retry_after_seconds: None,
        })
    }

    fn post_empty_status(
        &self,
        _url: &Url,
        token: &str,
    ) -> Result<GraphHttpStatus, GraphHttpError> {
        Ok(GraphHttpStatus {
            status: self.next("soft_delete", token),
            retry_after_seconds: None,
        })
    }

    fn get_content_redirect(
        &self,
        _url: &Url,
        token: &str,
    ) -> Result<GraphContentRedirect, GraphHttpError> {
        Ok(GraphContentRedirect {
            status: self.next("content", token),
            retry_after_seconds: None,
            location: None,
        })
    }
}

fn http_response(status: u16) -> GraphHttpResponse {
    GraphHttpResponse {
        status,
        retry_after_seconds: None,
        body: Vec::new(),
    }
}

fn tokens(access: &str, refresh: Option<&str>) -> ScriptedRefresh {
    let refresh = refresh
        .map(|value| format!(r#","refresh_token":"{value}""#))
        .unwrap_or_default();
    ScriptedRefresh::tokens(&format!(
        r#"{{"token_type":"Bearer","access_token":"{access}"{refresh},"expires_in":3600}}"#
    ))
}

struct Fixture {
    session: GraphSession<'static>,
    graph: GraphLog,
    token_posts: RecordedRefreshes,
}

fn fixture(
    graph_statuses: impl IntoIterator<Item = u16>,
    refresh_script: impl IntoIterator<Item = ScriptedRefresh>,
    expires_in: Duration,
) -> Fixture {
    let (refresher, token_posts) = refresher(GraphCloud::Global, "refresh-1", refresh_script);
    build(graph_statuses, Some(refresher), token_posts, expires_in)
}

fn build(
    graph_statuses: impl IntoIterator<Item = u16>,
    refresher: Option<TokenRefresher>,
    token_posts: RecordedRefreshes,
    expires_in: Duration,
) -> Fixture {
    let graph = GraphLog::default();
    let session = GraphSession::new(
        GraphCloud::Global,
        GraphCredentials::new(
            super::AccessToken::new("access-1".into()).expect("token"),
            Some(Instant::now() + expires_in),
            refresher,
        ),
        Box::new(ScriptedGraph {
            statuses: Mutex::new(graph_statuses.into_iter().collect()),
            log: Arc::clone(&graph),
        }),
    );
    Fixture {
        session,
        graph,
        token_posts,
    }
}

const LONG: Duration = Duration::from_secs(3600);

fn url() -> Url {
    Url::parse("https://graph.microsoft.com/v1.0/me/chats").expect("url")
}

fn graph_calls(fixture: &Fixture) -> Vec<(&'static str, String)> {
    fixture.graph.lock().expect("log").clone()
}

fn token_post_count(fixture: &Fixture) -> usize {
    fixture.token_posts.lock().expect("posts").len()
}

fn post(session: &GraphSession<'_>) -> Result<GraphHttpStatus, GraphHttpError> {
    let url = url();
    session.post_json_status(GraphJsonRequest {
        url: &url,
        content_type: "application/json",
        body: b"{}",
    })
}

#[test]
fn a_get_401_renews_and_retries_exactly_once() {
    let f = fixture([401, 200], [tokens("access-2", Some("refresh-2"))], LONG);
    let response = f.session.get_json(&url(), 1024).expect("read");
    assert_eq!(response.status, 200);
    assert_eq!(
        graph_calls(&f),
        [
            ("get_json", "access-1".to_string()),
            ("get_json", "access-2".to_string())
        ]
    );
    assert_eq!(token_post_count(&f), 1);
}

fn query(session: &GraphSession<'_>) -> Result<GraphHttpResponse, GraphHttpError> {
    let url = url();
    session.post_json_query(
        &GraphJsonRequest {
            url: &url,
            content_type: "application/json",
            body: b"{}",
        },
        1024,
    )
}

#[test]
fn post_json_query_is_read_class_and_retries_once_after_a_401_renewal() {
    let f = fixture([401, 200], [tokens("access-2", Some("refresh-2"))], LONG);
    assert_eq!(query(&f.session).expect("query").status, 200);
    assert_eq!(
        graph_calls(&f),
        [
            ("query", "access-1".to_string()),
            ("query", "access-2".to_string())
        ]
    );
    assert_eq!(token_post_count(&f), 1);
}

#[test]
fn post_json_query_never_makes_a_third_request() {
    let f = fixture([401, 401], [tokens("access-2", None)], LONG);
    assert_eq!(query(&f.session).expect("query").status, 401);
    assert_eq!(graph_calls(&f).len(), 2);
}

#[test]
fn every_read_class_renews_on_401() {
    let f = fixture(
        [401, 200, 401, 200],
        [tokens("access-2", None), tokens("access-3", None)],
        LONG,
    );
    assert_eq!(f.session.get_me().expect("me").status, 200);
    let redirect = f.session.get_content_redirect(&url()).expect("redirect");
    assert_eq!(redirect.status, 200);
    assert_eq!(token_post_count(&f), 2);
    let calls = graph_calls(&f);
    assert_eq!(calls[1], ("get_me", "access-2".to_string()));
    assert_eq!(calls[3], ("content", "access-3".to_string()));
}

#[test]
fn two_consecutive_401s_stop_with_no_third_request() {
    let f = fixture([401, 401], [tokens("access-2", None)], LONG);
    let response = f.session.get_json(&url(), 1024).expect("read");
    assert_eq!(response.status, 401);
    assert_eq!(graph_calls(&f).len(), 2);
    assert_eq!(token_post_count(&f), 1);
}

#[test]
fn security_post_401_is_never_replayed() {
    let f = fixture([401, 204], [tokens("access-2", Some("refresh-2"))], LONG);
    let error = post(&f.session).err().expect("must not be acknowledged");
    assert!(matches!(error, GraphHttpError::RenewedBeforeDelivery));
    // Exactly one Graph POST (with the old token) and one token POST.
    assert_eq!(graph_calls(&f), [("post", "access-1".to_string())]);
    assert_eq!(token_post_count(&f), 1);
}

#[test]
fn security_dispatch_after_post_401_is_a_known_rejection_with_one_post() {
    let (refresher, token_posts) = refresher(
        GraphCloud::Global,
        "refresh-1",
        [tokens("access-2", Some("refresh-2"))],
    );
    let f = build([401, 204], Some(refresher), token_posts, LONG);
    let Fixture {
        session,
        graph,
        token_posts,
    } = f;
    let mut transport = Microsoft365Transport::with_session(session);
    let error = transport
        .dispatch(WorkspaceCommand::SetChatMessageReaction {
            chat: ProviderResourceRef::microsoft365("chat"),
            message: ProviderResourceRef::microsoft365("message"),
            reaction: ReactionKind::Like,
            selected: true,
        })
        .expect_err("a 401 must not deliver");
    assert!(
        matches!(&error, TransportError::InvalidCommand(message) if message.contains("send again")),
        "{error:?}"
    );
    assert_eq!(graph.lock().expect("log").len(), 1);
    assert_eq!(token_posts.lock().expect("posts").len(), 1);
    // The renewal is not a sign-out.
    assert!(!matches!(
        transport.connection_status(),
        ConnectionStatus::ReauthenticationRequired
    ));
}

#[test]
fn the_renewed_token_serves_the_next_mutation() {
    let f = fixture([401, 204], [tokens("access-2", None)], LONG);
    assert!(post(&f.session).is_err());
    assert_eq!(post(&f.session).expect("second send").status, 204);
    assert_eq!(graph_calls(&f)[1], ("post", "access-2".to_string()));
    assert_eq!(token_post_count(&f), 1);
}

#[test]
fn a_near_expiry_token_is_renewed_before_a_post() {
    let f = fixture([204], [tokens("access-2", None)], Duration::from_secs(60));
    assert_eq!(post(&f.session).expect("send").status, 204);
    assert_eq!(graph_calls(&f), [("post", "access-2".to_string())]);
    assert_eq!(token_post_count(&f), 1);
}

#[test]
fn a_fresh_token_is_not_renewed_early() {
    let f = fixture([204, 200], [], LONG);
    assert_eq!(post(&f.session).expect("send").status, 204);
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 200);
    assert_eq!(token_post_count(&f), 0);
}

#[test]
fn skew_boundary_is_inclusive() {
    let now = Instant::now();
    assert!(refresh_due(now + TOKEN_REFRESH_SKEW, now));
    assert!(refresh_due(
        now + TOKEN_REFRESH_SKEW - Duration::from_millis(1),
        now
    ));
    assert!(!refresh_due(
        now + TOKEN_REFRESH_SKEW + Duration::from_millis(1),
        now
    ));
}

#[test]
fn a_rejected_grant_drops_the_refresher_and_never_calls_the_endpoint_again() {
    let f = fixture([401, 401, 401], [ScriptedRefresh::status(400)], LONG);
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 401);
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 401);
    assert_eq!(post(&f.session).expect("post").status, 401);
    assert_eq!(token_post_count(&f), 1);
    assert_eq!(graph_calls(&f).len(), 3);
}

#[test]
fn a_rejected_grant_during_a_post_401_reports_reauthentication() {
    let f = fixture([401], [ScriptedRefresh::status(400)], LONG);
    // The 401 status is passed through for the mutation classifier.
    assert_eq!(post(&f.session).expect("status").status, 401);
}

#[test]
fn a_server_error_keeps_the_refresher_for_the_next_attempt() {
    let f = fixture(
        [401, 401, 200],
        [ScriptedRefresh::status(503), tokens("access-2", None)],
        LONG,
    );
    assert!(matches!(
        f.session.get_json(&url(), 1),
        Err(GraphHttpError::Network)
    ));
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 200);
    assert_eq!(token_post_count(&f), 2);
}

#[test]
fn a_post_401_with_an_unreachable_endpoint_reports_not_delivered() {
    let f = fixture([401], [ScriptedRefresh::Network], LONG);
    assert!(matches!(
        post(&f.session),
        Err(GraphHttpError::RenewalUnavailable)
    ));
    assert_eq!(graph_calls(&f).len(), 1);
}

#[test]
fn a_throttled_renewal_surfaces_as_429() {
    let f = fixture(
        [401, 401],
        [ScriptedRefresh::status(429), ScriptedRefresh::status(429)],
        LONG,
    );
    let read = f.session.get_json(&url(), 1).expect("read");
    assert_eq!((read.status, read.retry_after_seconds), (429, Some(7)));
    let sent = post(&f.session).expect("status");
    assert_eq!((sent.status, sent.retry_after_seconds), (429, Some(7)));
    assert_eq!(graph_calls(&f).len(), 2);
}

#[test]
fn an_oversize_or_invalid_refresh_response_is_not_applied() {
    let f = fixture(
        [401, 401],
        [
            ScriptedRefresh::TooLarge,
            ScriptedRefresh::tokens(r#"{"token_type":"mac","access_token":"x","expires_in":60}"#),
        ],
        LONG,
    );
    assert!(f.session.get_json(&url(), 1).is_err());
    // Invalid response: rejected, refresher dropped, original 401 returned.
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 401);
    assert_eq!(
        graph_calls(&f)
            .iter()
            .filter(|(_, t)| t != "access-1")
            .count(),
        0
    );
}

#[test]
fn a_session_without_a_refresher_never_renews() {
    // The development-token shape: no refresher and no expiry.
    let f = build(
        [401, 401],
        None,
        RecordedRefreshes::default(),
        Duration::ZERO,
    );
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 401);
    assert_eq!(post(&f.session).expect("post").status, 401);
    assert_eq!(token_post_count(&f), 0);
}

#[test]
fn renewal_is_skipped_once_the_account_was_replaced() {
    let f = fixture([401], [tokens("access-2", None)], LONG);
    let cancellation = crate::integrations::TransportCancellation::new();
    cancellation.cancel();
    f.session.set_cancellation(Some(cancellation));
    assert!(f.session.get_json(&url(), 1).is_err());
    assert_eq!(token_post_count(&f), 0);
}

#[test]
fn dropping_the_session_drops_the_refresher() {
    // Account replacement drops the transport; nothing may use the refresh
    // token afterwards because nothing owns it.
    let f = fixture([], [], LONG);
    let Fixture {
        session,
        token_posts,
        ..
    } = f;
    drop(session);
    assert_eq!(token_posts.lock().expect("posts").len(), 0);
    assert_eq!(Arc::strong_count(&token_posts), 1);
}

const NEAR_EXPIRY: Duration = Duration::from_secs(10);

#[test]
fn a_throttled_preflight_renewal_backs_off_instead_of_retrying_every_request() {
    let f = fixture([200, 200, 200], [ScriptedRefresh::status(429)], NEAR_EXPIRY);
    for _ in 0..3 {
        assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 200);
    }
    // One throttled attempt; the other two requests skipped pre-flight.
    assert_eq!(token_post_count(&f), 1);
    assert!(graph_calls(&f).iter().all(|(_, token)| token == "access-1"));
}

#[test]
fn a_transient_preflight_failure_backs_off() {
    for failure in [ScriptedRefresh::Network, ScriptedRefresh::status(503)] {
        let f = fixture([204, 204], [failure], NEAR_EXPIRY);
        assert_eq!(post(&f.session).expect("send").status, 204);
        assert_eq!(post(&f.session).expect("send").status, 204);
        assert_eq!(token_post_count(&f), 1);
    }
}

#[test]
fn the_backoff_honours_a_long_retry_after_and_never_undercuts_the_minimum() {
    let mut credentials =
        GraphCredentials::unrenewable(super::AccessToken::new("t".into()).expect("token"));
    let (refresher, _) = refresher(
        GraphCloud::Global,
        "r",
        [ScriptedRefresh::Response {
            status: 429,
            body: "{}".into(),
        }],
    );
    credentials.refresher = Some(refresher);
    credentials.expires_at = Some(Instant::now());
    let f = fixture([], [], LONG);
    let before = Instant::now();
    f.session.renew_if_due(&mut credentials);
    let until = credentials.preflight_backoff_until.expect("backoff set");
    // The fake reports Retry-After 7s, below the 30s floor.
    assert!(until >= before + super::MIN_PREFLIGHT_BACKOFF);
    assert!(until <= Instant::now() + super::MIN_PREFLIGHT_BACKOFF);
}

#[test]
fn a_401_still_renews_during_a_preflight_backoff() {
    let f = fixture(
        [200, 401, 200],
        [ScriptedRefresh::status(503), tokens("access-2", None)],
        NEAR_EXPIRY,
    );
    // Pre-flight fails transiently and starts the backoff.
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 200);
    assert_eq!(token_post_count(&f), 1);
    // The 401 is the explicit path: it renews despite the backoff.
    assert_eq!(f.session.get_json(&url(), 1).expect("read").status, 200);
    assert_eq!(token_post_count(&f), 2);
    assert_eq!(graph_calls(&f)[2], ("get_json", "access-2".to_string()));
}

#[test]
fn a_successful_renewal_clears_the_backoff() {
    let f = fixture(
        [200, 401, 200],
        [ScriptedRefresh::status(503), tokens("access-2", None)],
        NEAR_EXPIRY,
    );
    f.session.get_json(&url(), 1).expect("read");
    f.session.get_json(&url(), 1).expect("read");
    assert!(
        f.session
            .lock_credentials()
            .preflight_backoff_until
            .is_none()
    );
}

fn transport_after_401(
    command: WorkspaceCommand,
) -> (TransportError, usize, usize, Microsoft365Transport) {
    let (refresher, token_posts) = refresher(
        GraphCloud::Global,
        "refresh-1",
        [tokens("access-2", Some("refresh-2"))],
    );
    let Fixture {
        session,
        graph,
        token_posts,
    } = build([401, 204], Some(refresher), token_posts, LONG);
    let mut transport = Microsoft365Transport::with_session(session);
    transport.identity = Some(crate::integrations::AccountIdentity {
        account_id: "acct".into(),
        display_name: "Ada".into(),
        email: None,
        tenant_id: None,
    });
    let error = transport
        .dispatch(command)
        .expect_err("a 401 must not deliver");
    let graph_requests = graph.lock().expect("log").len();
    let token_requests = token_posts.lock().expect("posts").len();
    (error, graph_requests, token_requests, transport)
}

#[test]
fn security_patch_401_is_never_replayed() {
    let (error, graph_requests, token_requests, transport) =
        transport_after_401(WorkspaceCommand::EditChatMessage {
            chat: ProviderResourceRef::microsoft365("chat"),
            message: ProviderResourceRef::microsoft365("message"),
            body: "new".into(),
        });
    assert!(
        matches!(&error, TransportError::InvalidCommand(message) if message.contains("send again")),
        "{error:?}"
    );
    // Exactly one Graph PATCH and one token POST; never a second PATCH.
    assert_eq!(graph_requests, 1);
    assert_eq!(token_requests, 1);
    assert!(!matches!(
        transport.connection_status(),
        ConnectionStatus::ReauthenticationRequired
    ));
}

#[test]
fn security_soft_delete_401_is_never_replayed() {
    let (error, graph_requests, token_requests, transport) =
        transport_after_401(WorkspaceCommand::DeleteChatMessage {
            chat: ProviderResourceRef::microsoft365("chat"),
            message: ProviderResourceRef::microsoft365("message"),
        });
    assert!(
        matches!(&error, TransportError::InvalidCommand(message) if message.contains("send again")),
        "{error:?}"
    );
    assert_eq!(graph_requests, 1);
    assert_eq!(token_requests, 1);
    assert!(!matches!(
        transport.connection_status(),
        ConnectionStatus::ReauthenticationRequired
    ));
}

#[test]
fn security_patch_and_soft_delete_401_log_one_request_with_the_old_token() {
    for patch in [true, false] {
        let f = fixture([401, 204], [tokens("access-2", None)], LONG);
        let url = url();
        let result = if patch {
            f.session.patch_json_status(GraphJsonRequest {
                url: &url,
                content_type: "application/json",
                body: b"{}",
            })
        } else {
            f.session.post_empty_status(&url)
        };
        assert!(matches!(
            result.err(),
            Some(GraphHttpError::RenewedBeforeDelivery)
        ));
        let kind = if patch { "patch" } else { "soft_delete" };
        assert_eq!(graph_calls(&f), [(kind, "access-1".to_string())]);
        assert_eq!(token_post_count(&f), 1);
    }
}

#[test]
fn a_renewal_updates_the_granted_scope_and_a_missing_scope_keeps_it() {
    let f = fixture(
        [401, 401],
        [
            ScriptedRefresh::tokens(
                r#"{"token_type":"Bearer","access_token":"access-2","expires_in":3600,"scope":"User.Read Chat.ReadWrite"}"#,
            ),
            tokens("access-3", None),
        ],
        LONG,
    );
    f.session.lock_credentials().granted_scope = Some("User.Read".into());
    assert!(post(&f.session).is_err());
    assert_eq!(
        f.session.granted_scope().as_deref(),
        Some("User.Read Chat.ReadWrite")
    );
    assert!(post(&f.session).is_err());
    assert_eq!(
        f.session.granted_scope().as_deref(),
        Some("User.Read Chat.ReadWrite")
    );
}
