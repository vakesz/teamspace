//! PKCE and correlation material for one interactive Entra authorization.
//!
//! The verifier stays in memory for the short-lived authorization exchange.
//! It is deliberately not `Debug`, serializable, or stored in a feature
//! snapshot.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::{Zeroize, Zeroizing};

const VERIFIER_RANDOM_BYTES: usize = 64;
const CORRELATION_RANDOM_BYTES: usize = 32;
const MIN_VERIFIER_BYTES: usize = 43;
const MAX_VERIFIER_BYTES: usize = 128;
const MAX_CORRELATION_BYTES: usize = 128;

/// One in-memory PKCE proof and the state value bound to its browser
/// redirect. Construct one per authorization attempt; never reuse it.
pub(super) struct PkceSession {
    code_verifier: String,
    code_challenge: String,
    state: String,
}

impl PkceSession {
    /// Obtains fresh randomness from the operating system and derives an S256
    /// PKCE challenge. Failure is explicit: callers must not fall back to
    /// deterministic or weak entropy.
    pub(super) fn generate() -> Result<Self, PkceError> {
        let mut verifier = random_url_safe(VERIFIER_RANDOM_BYTES)?;
        let mut state = random_url_safe(CORRELATION_RANDOM_BYTES)?;
        Self::from_values(std::mem::take(&mut *verifier), std::mem::take(&mut *state))
    }

    pub(super) fn from_values(
        mut code_verifier: String,
        mut state: String,
    ) -> Result<Self, PkceError> {
        if !is_verifier(&code_verifier) {
            code_verifier.zeroize();
            state.zeroize();
            return Err(PkceError::InvalidVerifier);
        }
        if !is_correlation_value(&state) {
            code_verifier.zeroize();
            state.zeroize();
            return Err(PkceError::InvalidState);
        }
        let digest = Sha256::digest(code_verifier.as_bytes());
        Ok(Self {
            code_challenge: URL_SAFE_NO_PAD.encode(digest),
            code_verifier,
            state,
        })
    }

    pub(super) fn code_challenge(&self) -> &str {
        &self.code_challenge
    }

    pub(super) fn state(&self) -> &str {
        &self.state
    }

    /// Exposes the verifier only to the later authorization-code exchange.
    /// Callers must keep the resulting request body in memory and must never
    /// log or persist this value.
    pub(super) fn code_verifier_for_exchange(&self) -> &str {
        &self.code_verifier
    }
}

impl Drop for PkceSession {
    fn drop(&mut self) {
        // The verifier is credential-adjacent and must not remain in a freed
        // heap allocation longer than its one authorization attempt. Clearing
        // state/challenge too keeps the entire short-lived correlation
        // record out of post-session memory inspection.
        self.code_verifier.zeroize();
        self.code_challenge.zeroize();
        self.state.zeroize();
    }
}

fn random_url_safe(byte_count: usize) -> Result<Zeroizing<String>, PkceError> {
    let mut bytes = Zeroizing::new(vec![0; byte_count]);
    getrandom::fill(&mut bytes).map_err(|_| PkceError::EntropyUnavailable)?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_slice())))
}

fn is_verifier(value: &str) -> bool {
    (MIN_VERIFIER_BYTES..=MAX_VERIFIER_BYTES).contains(&value.len())
        && value.bytes().all(is_pkce_character)
}

fn is_correlation_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CORRELATION_BYTES
        && value.bytes().all(is_pkce_character)
}

const fn is_pkce_character(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.' | b'_' | b'~')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PkceError {
    EntropyUnavailable,
    InvalidVerifier,
    InvalidState,
}

impl fmt::Display for PkceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => {
                formatter.write_str("could not obtain secure authorization randomness")
            }
            Self::InvalidVerifier => formatter.write_str("authorization verifier is invalid"),
            Self::InvalidState => formatter.write_str("authorization state is invalid"),
        }
    }
}

impl std::error::Error for PkceError {}

#[cfg(test)]
mod tests {
    use super::{PkceError, PkceSession};

    #[test]
    fn pkce_s256_challenge_matches_the_rfc_example() {
        let session = PkceSession::from_values(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            "state-with-at-least-some-safe-entropy".into(),
        )
        .expect("the RFC verifier must be valid");

        assert_eq!(
            session.code_challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            session.code_verifier_for_exchange(),
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
    }

    #[test]
    fn generated_sessions_have_distinct_rfc_sized_material() {
        let first = PkceSession::generate().expect("OS entropy must be available in tests");
        let second = PkceSession::generate().expect("OS entropy must be available in tests");

        assert!((43..=128).contains(&first.code_verifier_for_exchange().len()));
        assert_eq!(first.code_challenge().len(), 43);
        assert!(first.state().len() >= 43);
        assert_ne!(
            first.code_verifier_for_exchange(),
            second.code_verifier_for_exchange()
        );
        assert_ne!(first.state(), second.state());
    }

    #[test]
    fn invalid_values_are_rejected_before_an_authorization_url_can_be_built() {
        assert!(matches!(
            PkceSession::from_values("too-short".into(), "safe-state".into()),
            Err(PkceError::InvalidVerifier)
        ));
        assert!(matches!(
            PkceSession::from_values("a".repeat(43), "state with spaces".into()),
            Err(PkceError::InvalidState)
        ));
    }
}
