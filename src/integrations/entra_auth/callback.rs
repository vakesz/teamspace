//! Strict parsing for one Entra loopback redirect request.
//!
//! A future loopback listener must bind `127.0.0.1` before opening a browser,
//! read at most [`MAX_LOOPBACK_REQUEST_BYTES`], and pass the complete request
//! here. This parser does not start a listener or exchange a code; it keeps
//! the HTTP boundary small and independently testable.

use std::fmt;
use zeroize::{Zeroize, Zeroizing};

pub(super) const CALLBACK_PATH: &str = "/oauth/callback";
pub(super) const MAX_LOOPBACK_REQUEST_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_CODE_BYTES: usize = 4 * 1024;
const MAX_ERROR_BYTES: usize = 256;
const MAX_ERROR_DESCRIPTION_BYTES: usize = 2 * 1024;

/// An authorization result delivered by the correctly correlated loopback
/// callback. It intentionally has no `Debug` implementation: authorization
/// codes must never accidentally be emitted in diagnostics.
pub(super) enum AuthorizationCallback {
    AuthorizationCode {
        code: String,
    },
    AuthorizationError {
        error: String,
        description: Option<String>,
    },
}

impl AuthorizationCallback {
    pub(super) fn authorization_code(&self) -> Option<&str> {
        match self {
            Self::AuthorizationCode { code } => Some(code),
            Self::AuthorizationError { .. } => None,
        }
    }

    #[allow(dead_code)] // Reserved for surfacing an authorization-server error.
    pub(super) fn authorization_error(&self) -> Option<(&str, Option<&str>)> {
        match self {
            Self::AuthorizationCode { .. } => None,
            Self::AuthorizationError { error, description } => {
                Some((error, description.as_deref()))
            }
        }
    }
}

impl Drop for AuthorizationCallback {
    fn drop(&mut self) {
        match self {
            Self::AuthorizationCode { code } => code.zeroize(),
            Self::AuthorizationError { error, description } => {
                error.zeroize();
                if let Some(description) = description {
                    description.zeroize();
                }
            }
        }
    }
}

/// Parses a complete, bounded HTTP request sent to the exact loopback
/// callback. The expected state is required for both success and denial
/// responses, preventing unrelated local-browser requests from being treated
/// as an authorization result.
pub(super) fn parse_loopback_callback(
    request: &[u8],
    expected_state: &str,
    expected_port: u16,
) -> Result<AuthorizationCallback, LoopbackCallbackError> {
    if expected_port == 0 {
        return Err(LoopbackCallbackError::InvalidExpectedPort);
    }
    if !is_valid_expected_state(expected_state) {
        return Err(LoopbackCallbackError::InvalidExpectedState);
    }
    if request.len() > MAX_LOOPBACK_REQUEST_BYTES {
        return Err(LoopbackCallbackError::RequestTooLarge);
    }
    let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(LoopbackCallbackError::IncompleteRequest);
    };
    if headers_end + 4 != request.len() {
        return Err(LoopbackCallbackError::RequestBodyNotAllowed);
    }
    let headers = std::str::from_utf8(&request[..headers_end])
        .map_err(|_| LoopbackCallbackError::InvalidEncoding)?;
    if !uses_only_crlf_line_endings(headers) {
        return Err(LoopbackCallbackError::MalformedRequest);
    }
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(LoopbackCallbackError::MalformedRequest)?;
    let (method, target, version) = parse_request_line(request_line)?;
    if method != "GET" {
        return Err(LoopbackCallbackError::MethodNotAllowed);
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(LoopbackCallbackError::UnsupportedHttpVersion);
    }
    let host = parse_headers(lines)?;
    let expected_host = format!("127.0.0.1:{expected_port}");
    if host.as_deref() != Some(expected_host.as_str()) {
        return Err(LoopbackCallbackError::UnexpectedHost);
    }

    let query = parse_query(parse_callback_target(target)?)?;
    let state = query
        .state
        .as_deref()
        .ok_or(LoopbackCallbackError::MissingState)?;
    if !constant_time_equal(state, expected_state) {
        return Err(LoopbackCallbackError::StateMismatch);
    }
    match (query.code.as_deref(), query.error.as_deref()) {
        (Some(_), Some(_)) => Err(LoopbackCallbackError::ConflictingResult),
        (Some(code), None) => {
            if !is_safe_value(code, MAX_CODE_BYTES) {
                return Err(LoopbackCallbackError::InvalidAuthorizationCode);
            }
            Ok(AuthorizationCallback::AuthorizationCode {
                code: code.to_owned(),
            })
        }
        (None, Some(error)) => {
            if !is_safe_value(error, MAX_ERROR_BYTES) {
                return Err(LoopbackCallbackError::InvalidAuthorizationError);
            }
            if query
                .error_description
                .as_ref()
                .is_some_and(|description| !is_safe_value(description, MAX_ERROR_DESCRIPTION_BYTES))
            {
                return Err(LoopbackCallbackError::InvalidAuthorizationError);
            }
            Ok(AuthorizationCallback::AuthorizationError {
                error: error.to_owned(),
                description: query
                    .error_description
                    .as_deref()
                    .map(|description| description.to_owned()),
            })
        }
        (None, None) => Err(LoopbackCallbackError::MissingResult),
    }
}

fn parse_request_line(line: &str) -> Result<(&str, &str, &str), LoopbackCallbackError> {
    let parts = line.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(LoopbackCallbackError::MalformedRequest);
    }
    Ok((parts[0], parts[1], parts[2]))
}

fn uses_only_crlf_line_endings(headers: &str) -> bool {
    let bytes = headers.as_bytes();
    bytes.iter().enumerate().all(|(index, byte)| match byte {
        b'\r' => bytes.get(index + 1) == Some(&b'\n'),
        b'\n' => index > 0 && bytes[index - 1] == b'\r',
        _ => true,
    })
}

fn parse_headers<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Option<String>, LoopbackCallbackError> {
    let mut host = None;
    let mut count = 0;
    for line in lines {
        count += 1;
        if count > MAX_HEADERS {
            return Err(LoopbackCallbackError::TooManyHeaders);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(LoopbackCallbackError::MalformedRequest);
        };
        if !is_header_name(name) || value.chars().any(char::is_control) {
            return Err(LoopbackCallbackError::MalformedRequest);
        }
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return Err(LoopbackCallbackError::DuplicateHost);
            }
            host = Some(value.trim().to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            if value.trim() != "0" {
                return Err(LoopbackCallbackError::RequestBodyNotAllowed);
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // A callback has no request body. Reject framing that could make
            // the parser observe only a prefix of a streamed request.
            return Err(LoopbackCallbackError::RequestBodyNotAllowed);
        }
    }
    Ok(host)
}

fn is_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn parse_callback_target(target: &str) -> Result<&str, LoopbackCallbackError> {
    if !target.starts_with('/') || target.starts_with("//") || target.contains('#') {
        return Err(LoopbackCallbackError::InvalidTarget);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Err(LoopbackCallbackError::UnexpectedPath);
    }
    Ok(query)
}

struct CallbackQuery {
    state: Option<Zeroizing<String>>,
    code: Option<Zeroizing<String>>,
    error: Option<Zeroizing<String>>,
    error_description: Option<Zeroizing<String>>,
}

fn parse_query(raw_query: &str) -> Result<CallbackQuery, LoopbackCallbackError> {
    let mut query = CallbackQuery {
        state: None,
        code: None,
        error: None,
        error_description: None,
    };
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_name, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = decode_query_component(raw_name)?;
        let target = match name.as_str() {
            "state" => &mut query.state,
            "code" => &mut query.code,
            "error" => &mut query.error,
            "error_description" => &mut query.error_description,
            _ => continue,
        };
        let value = decode_query_component(raw_value)?;
        if target.replace(value).is_some() {
            return Err(LoopbackCallbackError::DuplicateQueryParameter);
        }
    }
    Ok(query)
}

fn decode_query_component(raw: &str) -> Result<Zeroizing<String>, LoopbackCallbackError> {
    let bytes = raw.as_bytes();
    let mut decoded = Zeroizing::new(Vec::with_capacity(bytes.len()));
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                let Some(high) = bytes.get(index + 1).copied().and_then(hex_value) else {
                    return Err(LoopbackCallbackError::InvalidTarget);
                };
                let Some(low) = bytes.get(index + 2).copied().and_then(hex_value) else {
                    return Err(LoopbackCallbackError::InvalidTarget);
                };
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    let value = std::str::from_utf8(&decoded).map_err(|_| LoopbackCallbackError::InvalidTarget)?;
    Ok(Zeroizing::new(value.to_owned()))
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn is_safe_value(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn is_valid_expected_state(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

/// These failures are deliberately category-only. Do not put a callback URL,
/// authorization code, state, or error description into diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoopbackCallbackError {
    InvalidExpectedPort,
    InvalidExpectedState,
    RequestTooLarge,
    IncompleteRequest,
    RequestBodyNotAllowed,
    InvalidEncoding,
    MalformedRequest,
    MethodNotAllowed,
    UnsupportedHttpVersion,
    TooManyHeaders,
    DuplicateHost,
    UnexpectedHost,
    InvalidTarget,
    UnexpectedPath,
    DuplicateQueryParameter,
    MissingState,
    StateMismatch,
    ConflictingResult,
    MissingResult,
    InvalidAuthorizationCode,
    InvalidAuthorizationError,
}

impl fmt::Display for LoopbackCallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExpectedPort => formatter.write_str("loopback callback port is invalid"),
            Self::InvalidExpectedState => {
                formatter.write_str("loopback callback expected state is invalid")
            }
            Self::RequestTooLarge => formatter.write_str("loopback callback request is too large"),
            Self::IncompleteRequest => {
                formatter.write_str("loopback callback request is incomplete")
            }
            Self::RequestBodyNotAllowed => {
                formatter.write_str("loopback callback request must not contain a body")
            }
            Self::InvalidEncoding => {
                formatter.write_str("loopback callback request has invalid encoding")
            }
            Self::MalformedRequest => formatter.write_str("loopback callback request is malformed"),
            Self::MethodNotAllowed => formatter.write_str("loopback callback must use GET"),
            Self::UnsupportedHttpVersion => {
                formatter.write_str("loopback callback HTTP version is unsupported")
            }
            Self::TooManyHeaders => formatter.write_str("loopback callback has too many headers"),
            Self::DuplicateHost => {
                formatter.write_str("loopback callback has duplicate Host headers")
            }
            Self::UnexpectedHost => {
                formatter.write_str("loopback callback host does not match the registered redirect")
            }
            Self::InvalidTarget => formatter.write_str("loopback callback target is invalid"),
            Self::UnexpectedPath => {
                formatter.write_str("loopback callback path does not match the registered redirect")
            }
            Self::DuplicateQueryParameter => {
                formatter.write_str("loopback callback has duplicate authorization parameters")
            }
            Self::MissingState => {
                formatter.write_str("loopback callback is missing authorization state")
            }
            Self::StateMismatch => formatter
                .write_str("loopback callback state does not match this authorization attempt"),
            Self::ConflictingResult => {
                formatter.write_str("loopback callback contains conflicting authorization results")
            }
            Self::MissingResult => {
                formatter.write_str("loopback callback is missing an authorization result")
            }
            Self::InvalidAuthorizationCode => {
                formatter.write_str("loopback callback authorization code is invalid")
            }
            Self::InvalidAuthorizationError => {
                formatter.write_str("loopback callback authorization error is invalid")
            }
        }
    }
}

impl std::error::Error for LoopbackCallbackError {}

#[cfg(test)]
mod tests {
    use super::{
        CALLBACK_PATH, LoopbackCallbackError, MAX_LOOPBACK_REQUEST_BYTES, parse_loopback_callback,
    };

    const STATE: &str = "state-with-at-least-some-safe-entropy";
    const PORT: u16 = 8400;

    fn request(target: &str, host: &str) -> Vec<u8> {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes()
    }

    #[test]
    fn accepts_a_bounded_matching_authorization_code_callback() {
        let callback = parse_loopback_callback(
            &request(
                &format!("{CALLBACK_PATH}?code=abc%2B123&state={STATE}"),
                "127.0.0.1:8400",
            ),
            STATE,
            PORT,
        )
        .expect("matching callback must parse");

        assert_eq!(callback.authorization_code(), Some("abc+123"));
        assert_eq!(callback.authorization_error(), None);
    }

    #[test]
    fn accepts_a_matching_authorization_denial_without_turning_it_into_a_code() {
        let callback = parse_loopback_callback(
            &request(
                &format!(
                    "{CALLBACK_PATH}?error=access_denied&error_description=User+cancelled&state={STATE}"
                ),
                "127.0.0.1:8400",
            ),
            STATE,
            PORT,
        )
        .expect("matching denial must parse");

        assert_eq!(callback.authorization_code(), None);
        assert_eq!(
            callback.authorization_error(),
            Some(("access_denied", Some("User cancelled")))
        );
    }

    #[test]
    fn rejects_wrong_method_host_path_and_state_before_exposing_a_result() {
        let post = format!(
            "POST {CALLBACK_PATH}?code=abc&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\n\r\n"
        );
        assert!(matches!(
            parse_loopback_callback(post.as_bytes(), STATE, PORT),
            Err(LoopbackCallbackError::MethodNotAllowed)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=abc&state={STATE}"),
                    "localhost:8400"
                ),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::UnexpectedHost)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    "/wrong?code=abc&state=state-with-at-least-some-safe-entropy",
                    "127.0.0.1:8400"
                ),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::UnexpectedPath)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=abc&state=other"),
                    "127.0.0.1:8400"
                ),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::StateMismatch)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=abc&state={STATE}"),
                    "127.0.0.1:8400",
                ),
                "",
                PORT,
            ),
            Err(LoopbackCallbackError::InvalidExpectedState)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=abc&state={STATE}"),
                    "127.0.0.1:8400",
                ),
                "state with spaces",
                PORT,
            ),
            Err(LoopbackCallbackError::InvalidExpectedState)
        ));
    }

    #[test]
    fn rejects_duplicate_conflicting_or_bodied_callbacks() {
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=first&code=second&state={STATE}"),
                    "127.0.0.1:8400",
                ),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::DuplicateQueryParameter)
        ));
        assert!(matches!(
            parse_loopback_callback(
                &request(
                    &format!("{CALLBACK_PATH}?code=abc&error=access_denied&state={STATE}"),
                    "127.0.0.1:8400",
                ),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::ConflictingResult)
        ));
        let mut bodied = request(
            &format!("{CALLBACK_PATH}?code=abc&state={STATE}"),
            "127.0.0.1:8400",
        );
        bodied.extend_from_slice(b"not allowed");
        assert!(matches!(
            parse_loopback_callback(&bodied, STATE, PORT),
            Err(LoopbackCallbackError::RequestBodyNotAllowed)
        ));

        for framing in ["Content-Length: 1", "Transfer-Encoding: chunked"] {
            let framed = format!(
                "GET {CALLBACK_PATH}?code=abc&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\n{framing}\r\n\r\n"
            );
            assert!(matches!(
                parse_loopback_callback(framed.as_bytes(), STATE, PORT),
                Err(LoopbackCallbackError::RequestBodyNotAllowed)
            ));
        }
    }

    #[test]
    fn rejects_oversized_or_malformed_callback_requests() {
        assert!(matches!(
            parse_loopback_callback(&vec![b'a'; MAX_LOOPBACK_REQUEST_BYTES + 1], STATE, PORT),
            Err(LoopbackCallbackError::RequestTooLarge)
        ));
        assert!(matches!(
            parse_loopback_callback(b"GET / HTTP/1.1\n\n", STATE, PORT),
            Err(LoopbackCallbackError::IncompleteRequest)
        ));
        assert!(matches!(
            parse_loopback_callback(
                format!(
                    "GET {CALLBACK_PATH}?code=abc&state={STATE} HTTP/1.1\r\nHost: 127.0.0.1:8400\r\nX-Test: a\nb\r\n\r\n"
                )
                .as_bytes(),
                STATE,
                PORT,
            ),
            Err(LoopbackCallbackError::MalformedRequest)
        ));
    }
}
