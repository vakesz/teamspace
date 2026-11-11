//! Shared outcome classification for Microsoft Graph mutations and the route
//! identifier guard used by every token-bearing route builder.
//!
//! Each mutation module owns its own route and payload, but they all apply
//! one status contract: only the documented success status proves delivery,
//! a fixed set of 4xx statuses proves Graph did not accept the write, and
//! everything else stays delivery-uncertain so a non-idempotent POST is never
//! replayed automatically.

use super::{GraphHttpError, GraphHttpStatus, Microsoft365Transport};
use crate::integrations::{CommandDisposition, CommandReceipt, TransportError};

/// Statuses that prove Graph refused the write. `413` belongs here: the
/// server rejects an oversized body before processing it, so the mutation
/// cannot have been applied and the feature may safely release its gate.
pub(super) const REJECTED_MUTATION_STATUSES: [u16; 5] = [400, 404, 409, 413, 422];

/// Whether a provider identifier would be silently dropped or collapsed by
/// `Url::path_segments_mut().push(..)` normalization, retargeting a request.
pub(super) fn is_dot_segment(id: &str) -> bool {
    matches!(id.trim(), "." | "..")
}

/// Rejects an identifier that must never become a route segment.
pub(super) fn reject_dot_segment(id: &str) -> Result<(), TransportError> {
    reject_dot_segments(&[id])
}

/// Rejects the whole set when any identifier is a dot segment.
pub(super) fn reject_dot_segments(ids: &[&str]) -> Result<(), TransportError> {
    if ids.iter().any(|id| is_dot_segment(id)) {
        return Err(TransportError::Unsupported(
            "Microsoft Graph identifier is not a valid provider ID".into(),
        ));
    }
    Ok(())
}

/// Fixed, presentation-safe messages for one mutation kind.
#[derive(Clone, Copy)]
pub(super) struct MutationMessages {
    pub(super) rejected: &'static str,
    pub(super) unconfirmed: &'static str,
}

impl Microsoft365Transport {
    /// Maps one status-only response from a non-idempotent mutation.
    pub(super) fn classify_mutation_status(
        &mut self,
        response: GraphHttpStatus,
        success_status: u16,
        command_id: &'static str,
        messages: MutationMessages,
    ) -> Result<CommandReceipt, TransportError> {
        match response.status {
            status if status == success_status => Ok(CommandReceipt {
                // The app correlates through its opaque intent ID; never
                // surface a Graph message, chat, team or channel ID here.
                command_id: command_id.into(),
                disposition: CommandDisposition::AppliedRemotely,
                upload: None,
            }),
            401 => {
                let error = TransportError::ReauthenticationRequired;
                self.identity = None;
                self.record_error(&error);
                Err(error)
            }
            // Missing consent, membership, moderation, licensing and
            // conditional access can all produce 403. Keep detail private.
            403 => Err(TransportError::Unauthorized),
            429 => Err(TransportError::Throttled {
                retry_after_seconds: response.retry_after_seconds,
            }),
            status if REJECTED_MUTATION_STATUSES.contains(&status) => {
                Err(TransportError::InvalidCommand(messages.rejected.into()))
            }
            // Network, 5xx and unrecognized outcomes cannot prove whether a
            // non-idempotent write reached Graph.
            _ => Err(TransportError::Unavailable(messages.unconfirmed.into())),
        }
    }
}

/// Maps a transport-level failure of a mutation to a delivery-uncertain error.
pub(super) fn map_mutation_http_error(
    error: GraphHttpError,
    unconfirmed: &'static str,
    unavailable: &'static str,
) -> TransportError {
    match error {
        GraphHttpError::Network | GraphHttpError::ResponseTooLarge => {
            TransportError::Unavailable(unconfirmed.into())
        }
        GraphHttpError::Unsupported | GraphHttpError::RenewalUnavailable => {
            TransportError::Unavailable(unavailable.into())
        }
        // The write got 401 and was never replayed; the session is already
        // renewed, so a fresh attempt is safe and the draft can be restored.
        GraphHttpError::RenewedBeforeDelivery => TransportError::InvalidCommand(
            "Microsoft 365 renewed the session before delivery; send again".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_segments_are_detected_after_trimming() {
        for id in [".", "..", " .. ", " . "] {
            assert!(is_dot_segment(id), "{id:?}");
            assert!(reject_dot_segment(id).is_err(), "{id:?}");
        }
        for id in ["a.b", "...", "..a", "19:abc@thread.tacv2", ""] {
            assert!(!is_dot_segment(id), "{id:?}");
            assert!(reject_dot_segment(id).is_ok(), "{id:?}");
        }
        assert!(reject_dot_segments(&["ok", "..", "ok"]).is_err());
        assert!(reject_dot_segments(&["ok", "also"]).is_ok());
    }

    #[test]
    fn rejection_set_includes_payload_too_large() {
        for status in [400, 404, 409, 413, 422] {
            assert!(REJECTED_MUTATION_STATUSES.contains(&status));
        }
        for status in [401, 403, 408, 429, 500, 502, 503] {
            assert!(!REJECTED_MUTATION_STATUSES.contains(&status));
        }
    }

    #[test]
    fn http_errors_stay_delivery_uncertain() {
        for error in [
            GraphHttpError::Network,
            GraphHttpError::ResponseTooLarge,
            GraphHttpError::Unsupported,
            GraphHttpError::RenewalUnavailable,
        ] {
            assert!(matches!(
                map_mutation_http_error(error, "unconfirmed", "unavailable"),
                TransportError::Unavailable(_)
            ));
        }
        assert!(matches!(
            map_mutation_http_error(
                GraphHttpError::RenewedBeforeDelivery,
                "unconfirmed",
                "unavailable"
            ),
            TransportError::InvalidCommand(message) if message.contains("send again")
        ));
    }
}
