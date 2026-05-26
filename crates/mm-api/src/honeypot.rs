//! Honeypot field check. The client always sends `website: ""`. A non-empty
//! value indicates a bot scraping the form — we reject with a generic 422
//! and never tell the client why (so bot operators don't learn the trigger).

use crate::state::AppState;
use mm_core::error::{ErrorCode, MMError};

/// Check the honeypot field value. Returns `Ok(())` when the field is empty
/// (expected from real clients), or an opaque `MMError` when it is non-empty.
///
/// The counter `signup_honeypot_hits` is incremented on every rejection so
/// bot traffic can be tracked in Prometheus without exposing the mechanism
/// to the client.
pub fn check(state: &AppState, honeypot_value: &str) -> Result<(), MMError> {
    if !honeypot_value.is_empty() {
        state.metrics.signup_honeypot_hits.inc();
        return Err(MMError::api(ErrorCode::HoneypotFilled, "Bot detected"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Integration-tested via the signup handler test in Task B2.
}
