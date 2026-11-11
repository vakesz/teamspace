//! Credential-free building blocks for tenant-configured Entra sign-in.
//!
//! The app drives this module only through its explicit Microsoft 365 connect
//! flow. It can parse a tenant-owned public-client configuration, prepare a
//! PKCE authorization request, bind a verified loopback callback, and exchange
//! a returned code into in-memory tokens. It never persists a token or changes
//! workspace state on its own.

pub(crate) mod authorization;
mod callback;
pub(crate) mod config;
mod listener;
mod pkce;
mod token;
#[cfg(test)]
pub(in crate::integrations) use token::test_support;

/// Fuzzing seam: parses `request` as a loopback callback and reports whether it
/// was accepted. An accepted request must have carried the expected state
/// verbatim, because the state alphabet is URL-safe and never percent-encoded.
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_loopback_callback(request: &[u8], expected_state: &str, port: u16) -> bool {
    let accepted = callback::parse_loopback_callback(request, expected_state, port).is_ok();
    if accepted {
        assert!(
            request
                .windows(expected_state.len())
                .any(|window| window == expected_state.as_bytes()),
            "an accepted callback must contain the expected state"
        );
    }
    accepted
}
