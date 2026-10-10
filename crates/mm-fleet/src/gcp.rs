//! Google Cloud (Compute Engine) read-only checks: the 5-minute check and Test connection.
//!
//! SCAFFOLD: `checker` is the fixed entry point `adapters::checker_for` calls. The checker
//! below still answers "not built yet" and is replaced by the real implementation.

use crate::checks::{CheckReport, CheckState, ProviderChecker};
use crate::sealed::CredentialPlaintext;

/// The checker for one Google Cloud provider. `zones` is `(zone, sizes)` in failover order;
/// `stand_in_base` is a test base URL that replaces every Google host (tests only).
pub fn checker(
    _pt: &CredentialPlaintext,
    _zones: Vec<(String, Vec<String>)>,
    _stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    Box::new(NotBuilt)
}

struct NotBuilt;

#[async_trait::async_trait]
impl ProviderChecker for NotBuilt {
    async fn check(&self) -> CheckReport {
        let mut r = CheckReport::new_ok();
        r.state = CheckState::Unknown;
        r.last_error = Some((
            "unsupported".into(),
            "checks for this provider are not built yet".into(),
        ));
        r
    }
}
