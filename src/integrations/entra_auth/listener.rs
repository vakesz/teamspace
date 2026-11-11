//! A bounded loopback receiver for one Entra authorization redirect.
//!
//! The listener binds the exact registered `127.0.0.1` redirect before a
//! browser URL is handed to a caller. It accepts only a small number of
//! complete, header-only requests and lets the request-owned parser validate
//! host, path, and PKCE state. Dropping the listener closes its port, which is
//! the primary cancellation path when an interactive sign-in is abandoned.

use super::authorization::EntraAuthorizationRequest;
use super::callback::{AuthorizationCallback, MAX_LOOPBACK_REQUEST_BYTES};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const MAX_CALLBACK_WAIT: Duration = Duration::from_secs(5 * 60);
const MAX_SINGLE_CALLBACK_READ: Duration = Duration::from_millis(500);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Concurrent unfinished connections. Beyond this the oldest is dropped.
const MAX_OPEN_CONNECTIONS: usize = 8;
/// Accepts handled before the wait loop re-checks its deadline/cancellation.
const MAX_ACCEPTS_PER_PASS: usize = MAX_OPEN_CONNECTIONS * 2;
const READ_CHUNK_BYTES: usize = 1_024;
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

const CALLBACK_RECEIVED_BODY: &[u8] = b"<!doctype html><meta charset=\"utf-8\"><title>Teamspace</title><p>Sign-in response received. You may return to Teamspace.</p>";
const CALLBACK_REJECTED_BODY: &[u8] = b"<!doctype html><meta charset=\"utf-8\"><title>Teamspace</title><p>This sign-in callback could not be accepted. Return to Teamspace.</p>";

/// A cloneable cancellation signal for one waiting authorization session.
///
/// It contains no token, code, verifier, or correlation value. The eventual
/// app UI can retain this while a worker waits for a browser callback and
/// request cancellation without touching the secret-bearing request object.
#[derive(Clone, Default)]
pub(crate) struct AuthorizationCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AuthorizationCancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// One bound, nonblocking listener for a request-owned loopback callback.
///
/// This type intentionally remains inside `entra_auth`: callers receive an
/// authorization session rather than raw HTTP bytes or a state value.
pub(super) struct LoopbackCallbackListener {
    listener: TcpListener,
}

impl LoopbackCallbackListener {
    pub(super) fn bind(request: &EntraAuthorizationRequest) -> Result<Self, LoopbackListenerError> {
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, request.loopback_port());
        // Only the `io::ErrorKind` category is kept: the address is a fixed
        // loopback port and the kind (e.g. `AddrInUse`) helps users diagnose.
        let listener = TcpListener::bind(address)
            .map_err(|error| LoopbackListenerError::BindFailed(error.kind()))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| LoopbackListenerError::ConfigureFailed)?;
        Ok(Self { listener })
    }

    pub(super) fn wait_for_callback(
        &self,
        request: &EntraAuthorizationRequest,
        timeout: Duration,
        cancellation: &AuthorizationCancellation,
    ) -> Result<AuthorizationCallback, LoopbackListenerError> {
        if timeout.is_zero() || timeout > MAX_CALLBACK_WAIT {
            return Err(LoopbackListenerError::InvalidTimeout);
        }

        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(LoopbackListenerError::InvalidTimeout)?;

        let mut connections: Vec<PendingConnection> = Vec::with_capacity(MAX_OPEN_CONNECTIONS);
        loop {
            if cancellation.is_cancelled() {
                return Err(LoopbackListenerError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LoopbackListenerError::TimedOut);
            }

            let mut progressed = self.accept_pending(&mut connections, deadline)?;

            // Every open connection advances independently each pass, so a
            // peer that stalls (or trickles bytes) never delays a complete
            // request on another connection.
            let mut index = 0;
            while index < connections.len() {
                let state = connections[index].advance(deadline);
                match state {
                    ConnectionState::Waiting => index += 1,
                    ConnectionState::Failed => {
                        progressed = true;
                        let mut rejected = connections.swap_remove(index);
                        write_response(
                            &mut rejected.stream,
                            "400 Bad Request",
                            CALLBACK_REJECTED_BODY,
                        );
                    }
                    ConnectionState::Complete => {
                        progressed = true;
                        let mut finished = connections.swap_remove(index);
                        match request.parse_callback(&finished.buffer) {
                            Ok(callback) => {
                                if cancellation.is_cancelled() {
                                    return Err(LoopbackListenerError::Cancelled);
                                }
                                if Instant::now() >= deadline {
                                    return Err(LoopbackListenerError::TimedOut);
                                }
                                write_response(
                                    &mut finished.stream,
                                    "200 OK",
                                    CALLBACK_RECEIVED_BODY,
                                );
                                return Ok(callback);
                            }
                            Err(_) => {
                                // Junk or foreign connections never count toward an
                                // abort: only the overall timeout and explicit
                                // cancellation end a sign-in.
                                write_response(
                                    &mut finished.stream,
                                    "400 Bad Request",
                                    CALLBACK_REJECTED_BODY,
                                );
                            }
                        }
                    }
                }
            }

            if !progressed {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(ACCEPT_POLL_INTERVAL.min(remaining));
            }
        }
    }

    /// Accepts every queued connection. At the cap the oldest open connection
    /// is closed to make room, so a flood of idle peers can occupy at most
    /// `MAX_OPEN_CONNECTIONS` slots and a fresh (likely real) one is always
    /// admitted. Returns whether any connection was accepted.
    fn accept_pending(
        &self,
        connections: &mut Vec<PendingConnection>,
        deadline: Instant,
    ) -> Result<bool, LoopbackListenerError> {
        let mut accepted = false;
        // Bounded per pass so a local connection flood cannot keep the caller
        // from re-checking its deadline and cancellation signal.
        for _ in 0..MAX_ACCEPTS_PER_PASS {
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    accepted = true;
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    if connections.len() >= MAX_OPEN_CONNECTIONS {
                        let _ = connections.remove(0).stream.shutdown(Shutdown::Both);
                    }
                    connections.push(PendingConnection::new(stream, deadline));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(accepted),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(LoopbackListenerError::AcceptFailed),
            }
        }
        Ok(accepted)
    }

    #[cfg(test)]
    fn from_bound_listener(listener: TcpListener) -> Self {
        listener
            .set_nonblocking(true)
            .expect("test listener must become nonblocking");
        Self { listener }
    }

    #[cfg(test)]
    fn address(&self) -> SocketAddrV4 {
        match self
            .listener
            .local_addr()
            .expect("test listener must have an address")
        {
            std::net::SocketAddr::V4(address) => address,
            std::net::SocketAddr::V6(_) => panic!("test listener must use IPv4"),
        }
    }
}

enum ConnectionState {
    Waiting,
    /// A complete, header-only request is buffered.
    Complete,
    Failed,
}

/// One accepted connection and the bounded request bytes read so far.
struct PendingConnection {
    stream: TcpStream,
    buffer: Zeroizing<Vec<u8>>,
    deadline: Instant,
}

impl PendingConnection {
    fn new(stream: TcpStream, overall_deadline: Instant) -> Self {
        let deadline = Instant::now()
            .checked_add(MAX_SINGLE_CALLBACK_READ)
            .map_or(overall_deadline, |limit| limit.min(overall_deadline));
        Self {
            stream,
            // Sized for the largest accepted request plus one read chunk, so
            // the buffer never reallocates and leaves a stale copy of the
            // (secret-bearing) request in freed memory.
            buffer: Zeroizing::new(Vec::with_capacity(
                MAX_LOOPBACK_REQUEST_BYTES + READ_CHUNK_BYTES,
            )),
            deadline,
        }
    }

    /// Reads whatever is available without blocking.
    fn advance(&mut self, overall_deadline: Instant) -> ConnectionState {
        let mut chunk = Zeroizing::new([0_u8; READ_CHUNK_BYTES]);
        loop {
            match self.stream.read(&mut *chunk) {
                // The peer closed: parse whatever arrived.
                Ok(0) => return ConnectionState::Complete,
                Ok(count) => {
                    self.buffer.extend_from_slice(&chunk[..count]);
                    if self.buffer.len() > MAX_LOOPBACK_REQUEST_BYTES {
                        return Self::fail(LoopbackListenerError::RequestTooLarge);
                    }
                    // The parser rejects body framing, so a complete header
                    // block is the whole request.
                    if self.buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        return ConnectionState::Complete;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    return if now >= self.deadline || now >= overall_deadline {
                        Self::fail(LoopbackListenerError::ReadTimedOut)
                    } else {
                        ConnectionState::Waiting
                    };
                }
                Err(_) => return Self::fail(LoopbackListenerError::ReadFailed),
            }
        }
    }

    /// The category is only for the call site's benefit; every failed
    /// connection receives the same generic rejection page.
    fn fail(_reason: LoopbackListenerError) -> ConnectionState {
        ConnectionState::Failed
    }
}

fn write_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
    let header = format!(
        "HTTP/1.1 {status}\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Cache-Control: no-store\r\n\
Pragma: no-cache\r\n\
Referrer-Policy: no-referrer\r\n\
X-Content-Type-Options: nosniff\r\n\
Connection: close\r\n\
Content-Length: {}\r\n\
\r\n",
        body.len()
    );
    // Accepted streams are nonblocking for reading; a nonblocking `write_all`
    // can fail halfway with `WouldBlock` and truncate the page. Switch to a
    // bounded blocking write so the whole response is delivered.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT));
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// Category-only listener failures. They never contain a callback URL,
/// authorization code, state, peer address, or operating-system message; the
/// bind failure keeps only its `io::ErrorKind` category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopbackListenerError {
    BindFailed(io::ErrorKind),
    ConfigureFailed,
    InvalidTimeout,
    Cancelled,
    TimedOut,
    RequestTooLarge,
    ReadTimedOut,
    ReadFailed,
    AcceptFailed,
}

impl fmt::Display for LoopbackListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BindFailed(_) => {
                formatter.write_str("could not bind the Entra loopback redirect")
            }
            Self::ConfigureFailed => {
                formatter.write_str("could not configure the Entra loopback redirect")
            }
            Self::InvalidTimeout => formatter.write_str("Entra callback timeout is invalid"),
            Self::Cancelled => formatter.write_str("Entra sign-in was cancelled"),
            Self::TimedOut => formatter.write_str("Entra sign-in callback timed out"),
            Self::RequestTooLarge => formatter.write_str("Entra callback request is too large"),
            Self::ReadTimedOut => formatter.write_str("Entra callback request timed out"),
            Self::ReadFailed => formatter.write_str("could not read the Entra callback request"),
            Self::AcceptFailed => {
                formatter.write_str("could not accept the Entra callback request")
            }
        }
    }
}

impl std::error::Error for LoopbackListenerError {}

#[cfg(test)]
mod tests {
    use super::{AuthorizationCancellation, LoopbackCallbackListener, LoopbackListenerError};
    use crate::integrations::entra_auth::authorization::EntraAuthorizationRequest;
    use crate::integrations::entra_auth::config::EntraPublicClientConfig;
    use crate::integrations::entra_auth::pkce::PkceSession;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    const CLIENT_ID: &str = "6f2c8b6a-218c-4d3f-b4f4-e8d77d6c9a72";
    const STATE: &str = "state-with-at-least-some-safe-entropy";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

    fn request_and_listener() -> (EntraAuthorizationRequest, LoopbackCallbackListener) {
        let raw_listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("test listener must bind");
        let port = raw_listener
            .local_addr()
            .expect("test listener must have an address")
            .port();
        let config = EntraPublicClientConfig::from_values(
            Some(CLIENT_ID),
            Some("organizations"),
            Some(&port.to_string()),
            Some("offline_access User.Read"),
        )
        .expect("test configuration must parse");
        let request = EntraAuthorizationRequest::new(
            config,
            PkceSession::from_values(VERIFIER.into(), STATE.into()).expect("test PKCE must parse"),
        );
        (
            request,
            LoopbackCallbackListener::from_bound_listener(raw_listener),
        )
    }

    fn callback_request(port: u16, target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
    }

    #[test]
    fn listener_returns_only_a_matching_callback_and_a_generic_success_page() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("test client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=secret-code&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("test callback must write");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("test response must read");
            response
        });

        let cancellation = AuthorizationCancellation::default();
        let callback = listener
            .wait_for_callback(&request, Duration::from_secs(1), &cancellation)
            .expect("matching callback must be accepted");
        assert_eq!(callback.authorization_code(), Some("secret-code"));
        let response = client.join().expect("test client must finish");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Sign-in response received"));
        assert!(!response.contains("secret-code"));
    }

    #[test]
    fn listener_rejects_unrelated_callbacks_and_keeps_waiting_for_the_matching_one() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let client = thread::spawn(move || {
            let targets = [
                "/wrong?code=wrong&state=wrong".to_owned(),
                format!("/oauth/callback?code=accepted&state={STATE}"),
            ];
            for target in targets {
                let mut stream = TcpStream::connect(address).expect("test client must connect");
                stream
                    .write_all(callback_request(address.port(), &target).as_bytes())
                    .expect("test callback must write");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("test response must read");
                if target.starts_with("/wrong") {
                    assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
                }
            }
        });

        let callback = listener
            .wait_for_callback(
                &request,
                Duration::from_secs(1),
                &AuthorizationCancellation::default(),
            )
            .expect("matching callback must still be accepted");
        assert_eq!(callback.authorization_code(), Some("accepted"));
        client.join().expect("test client must finish");
    }

    #[test]
    fn junk_connections_never_abort_a_sign_in() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let client = thread::spawn(move || {
            for index in 0..20 {
                let mut stream = TcpStream::connect(address).expect("test client must connect");
                let target = if index % 2 == 0 {
                    "/wrong?code=wrong&state=wrong".to_owned()
                } else {
                    "/oauth/callback?code=x&state=not-the-state".to_owned()
                };
                stream
                    .write_all(callback_request(address.port(), &target).as_bytes())
                    .expect("junk request must write");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("junk response must read");
                assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
                assert!(!response.contains("not-the-state"));
            }
            let mut stream = TcpStream::connect(address).expect("test client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=accepted&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("valid callback must write");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });

        let callback = listener
            .wait_for_callback(
                &request,
                Duration::from_secs(5),
                &AuthorizationCancellation::default(),
            )
            .expect("valid callback must be accepted after many junk requests");
        assert_eq!(callback.authorization_code(), Some("accepted"));
        client.join().expect("test client must finish");
    }

    #[test]
    fn junk_connections_do_not_prevent_the_timeout() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        for _ in 0..12 {
            let mut stream = TcpStream::connect(address).expect("test client must connect");
            stream
                .write_all(b"garbage\r\n\r\n")
                .expect("junk must write");
        }
        let started = Instant::now();
        assert!(matches!(
            listener.wait_for_callback(
                &request,
                Duration::from_millis(300),
                &AuthorizationCancellation::default()
            ),
            Err(LoopbackListenerError::TimedOut)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn listener_honors_cancellation_without_waiting_for_a_socket() {
        let (request, listener) = request_and_listener();
        let cancellation = AuthorizationCancellation::default();
        cancellation.cancel();

        assert!(matches!(
            listener.wait_for_callback(&request, Duration::from_secs(1), &cancellation),
            Err(LoopbackListenerError::Cancelled)
        ));
    }

    #[test]
    fn stalled_callback_is_rejected_before_it_can_block_a_matching_callback() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let stalled = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("stalled client must connect");
            stream
                .write_all(b"GET /oauth/callback?code=partial")
                .expect("stalled client must write a partial request");
            ready_sender
                .send(())
                .expect("stalled client must signal readiness");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("stalled client must observe closure");
            response
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("stalled client must queue before the valid callback");

        let valid = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("valid client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=accepted&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("valid callback must write");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("valid client must read its response");
            response
        });

        let started = Instant::now();
        let callback = listener
            .wait_for_callback(
                &request,
                Duration::from_secs(4),
                &AuthorizationCancellation::default(),
            )
            .expect("matching callback must be accepted despite the stalled peer");
        assert_eq!(callback.authorization_code(), Some("accepted"));
        // The stalled peer no longer holds up the real callback at all.
        assert!(started.elapsed() < Duration::from_millis(400));
        // Returning drops the listener, which closes the stalled connection
        // without a page rather than leaving it hanging.
        let stalled_response = stalled.join().expect("stalled client must finish");
        assert!(!stalled_response.contains("200 OK"));
        assert!(
            valid
                .join()
                .expect("valid client must finish")
                .starts_with("HTTP/1.1 200 OK")
        );
    }

    #[test]
    fn listener_honors_cancellation_while_a_peer_is_stalled() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let stalled = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("stalled client must connect");
            stream
                .write_all(b"GET /oauth/callback?code=partial")
                .expect("stalled client must write a partial request");
            ready_sender
                .send(())
                .expect("stalled client must signal readiness");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("stalled client must observe closure");
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("stalled client must queue before cancellation");

        let cancellation = AuthorizationCancellation::default();
        let cancellation_sender = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancellation_sender.cancel();
        });
        let started = Instant::now();
        assert!(matches!(
            listener.wait_for_callback(&request, Duration::from_secs(4), &cancellation),
            Err(LoopbackListenerError::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        canceller.join().expect("canceller must finish");
        stalled.join().expect("stalled client must finish");
    }

    #[test]
    fn listener_rejects_zero_and_unbounded_waits() {
        let (request, listener) = request_and_listener();
        let cancellation = AuthorizationCancellation::default();

        assert!(matches!(
            listener.wait_for_callback(&request, Duration::ZERO, &cancellation),
            Err(LoopbackListenerError::InvalidTimeout)
        ));
        assert!(matches!(
            listener.wait_for_callback(&request, Duration::from_secs(301), &cancellation),
            Err(LoopbackListenerError::InvalidTimeout)
        ));
    }

    #[test]
    fn many_silent_peers_cannot_starve_the_real_callback() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        // Far more silent peers than the listener keeps open at once.
        let silent: Vec<TcpStream> = (0..40)
            .map(|_| TcpStream::connect(address).expect("silent client must connect"))
            .collect();
        let client = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut stream = TcpStream::connect(address).expect("valid client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=accepted&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("valid callback must write");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            response
        });

        let started = Instant::now();
        let callback = listener
            .wait_for_callback(
                &request,
                Duration::from_secs(5),
                &AuthorizationCancellation::default(),
            )
            .expect("the real callback must be accepted among silent peers");

        assert_eq!(callback.authorization_code(), Some("accepted"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let response = client.join().expect("valid client must finish");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        drop(silent);
    }

    #[test]
    fn a_trickling_peer_does_not_delay_a_complete_request() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let mut trickle = TcpStream::connect(address).expect("trickle client must connect");
        trickle.write_all(b"GET /oauth").expect("partial write");
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("valid client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=accepted&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("valid callback must write");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });
        let started = Instant::now();
        let callback = listener
            .wait_for_callback(
                &request,
                Duration::from_secs(5),
                &AuthorizationCancellation::default(),
            )
            .expect("complete request must be accepted");
        assert_eq!(callback.authorization_code(), Some("accepted"));
        assert!(started.elapsed() < Duration::from_millis(400));
        client.join().expect("valid client must finish");
        drop(trickle);
    }

    #[test]
    fn the_success_page_is_delivered_in_full() {
        let (request, listener) = request_and_listener();
        let address = listener.address();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("client must connect");
            stream
                .write_all(
                    callback_request(
                        address.port(),
                        &format!("/oauth/callback?code=accepted&state={STATE}"),
                    )
                    .as_bytes(),
                )
                .expect("callback must write");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("response must read to the end");
            response
        });
        listener
            .wait_for_callback(
                &request,
                Duration::from_secs(2),
                &AuthorizationCancellation::default(),
            )
            .expect("callback must be accepted");
        let response = client.join().expect("client must finish");
        assert!(
            response.ends_with("You may return to Teamspace.</p>"),
            "{response}"
        );
    }
}
