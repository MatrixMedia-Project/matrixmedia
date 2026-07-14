//! One shared, correctly-configured outbound HTTP client.
//!
//! Every outbound caller in the workspace used to do `reqwest::Client::new()`, which
//! has **no timeouts at all**. A wedged dependency (Synapse, mm-switch, LNBits — any
//! of them) therefore hung the calling handler indefinitely: the request never
//! failed, it just never came back, and the axum worker stayed parked. That is how a
//! single slow dependency turns into a total outage.
//!
//! Building a client per request is also wasteful: each one carries its own
//! connection pool and TLS session cache, so nothing is ever reused and every call
//! pays a fresh TCP + TLS handshake.
//!
//! `shared()` fixes both: one process-wide client, built once, with deadlines.
//!
//! Timeouts are deliberately generous. The goal is to bound the worst case, not to
//! police latency — a limit tight enough to sever a slow-but-valid call would be a
//! regression, so these are set well above anything a healthy dependency does.

use std::sync::OnceLock;
use std::time::Duration;

/// Cap on establishing the TCP + TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on the whole request/response cycle, connection included.
///
/// Every consumer of this client (Synapse admin/client API, mm-switch control plane,
/// LNBits) exchanges small JSON payloads; none legitimately runs for 30s. Bulk media
/// transfers deliberately do NOT use this client.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle connections are kept this long for reuse.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

static SHARED: OnceLock<reqwest::Client> = OnceLock::new();

/// The process-wide outbound HTTP client.
///
/// Cheap to call repeatedly: `reqwest::Client` is an `Arc` internally, and this
/// hands back a reference to a single instance, so callers share one connection pool.
///
/// A per-request deadline can still be tightened where it matters — the request
/// builder's `.timeout()` overrides the client default for that call.
pub fn shared() -> &'static reqwest::Client {
    SHARED.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build()
            // A client with no timeouts is strictly worse than none at all, but
            // failing to build one is not a reason to take the process down; fall
            // back to the default so behaviour degrades rather than dies.
            .unwrap_or_else(|e| {
                tracing::error!(
                    error = %e,
                    "failed to build the shared HTTP client; falling back to an \
                     untimed default — outbound calls can hang"
                );
                reqwest::Client::new()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_client_is_a_singleton() {
        // Same pool for every caller — otherwise connection reuse never happens.
        let a = shared();
        let b = shared();
        assert!(
            std::ptr::eq(a, b),
            "shared() must hand back one process-wide client"
        );
    }

    #[test]
    fn timeouts_are_bounded() {
        // Guards the actual regression: a client with no deadlines lets a wedged
        // dependency park an axum worker forever.
        assert!(CONNECT_TIMEOUT.as_secs() > 0 && CONNECT_TIMEOUT.as_secs() <= 10);
        assert!(REQUEST_TIMEOUT.as_secs() > 0 && REQUEST_TIMEOUT.as_secs() <= 60);
        assert!(
            CONNECT_TIMEOUT < REQUEST_TIMEOUT,
            "the connect budget must fit inside the overall request budget"
        );
    }
}
