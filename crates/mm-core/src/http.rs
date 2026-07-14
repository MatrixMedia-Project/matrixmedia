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
/// 60s, not 30s: this is a BACKSTOP against a hung dependency, not a latency policy, and a
/// backstop must not be tighter than a dependency's own declared budget or it starts
/// failing calls that would have succeeded.
///
/// The binding constraint is mm-switch, whose HTTP server runs a `WriteTimeout` of 60s
/// (services/mm-switch/main.go) — an explicit statement that a handler there may take that
/// long. `POST /api/sources` in particular does a synchronous `preloadKeyframe()` which can
/// fetch a remote ad asset over HTTP. A 30s cap here would abort such a call at 30s where
/// it previously had no deadline at all: trading a hang for a new failure.
///
/// Anything that genuinely needs a tighter bound sets it per request — the auth path does
/// exactly that (5s on whoami), because auth sits in front of every request and must fail
/// fast. `RequestBuilder::timeout` REPLACES this default for that call; it does not
/// intersect with it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Send a request on behalf of `dependency`, recording latency and outcome.
///
/// `dependency` must be a fixed `&'static str` (`synapse`, `mm_switch`, `lnbits`) — it
/// becomes a Prometheus label, so a value derived from user input would blow up
/// cardinality.
///
/// The result is passed through untouched: this only observes. Callers keep their own
/// error handling, and a caller that does not want the metric can still use
/// [`shared()`] directly.
///
/// An `Ok` response with a 4xx/5xx status counts as `http_error`, not `ok` — from the
/// caller's perspective the dependency failed, and a series that calls a wall of 500s
/// "ok" is worse than no series at all.
pub async fn send(
    dependency: &'static str,
    req: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    let started = std::time::Instant::now();
    let result = req.send().await;

    let outcome = match &result {
        Ok(r) if r.status().is_success() => "ok",
        Ok(_) => "http_error",
        Err(e) if e.is_timeout() => "timeout",
        Err(_) => "transport_error",
    };
    crate::metrics_global::OUTBOUND_REQUEST_DURATION
        .with_label_values(&[dependency, outcome])
        .observe(started.elapsed().as_secs_f64());

    result
}

/// Label used for calls to the Matrix homeserver (whoami, admin API, appservice).
pub const DEP_SYNAPSE: &str = "synapse";
/// Label used for calls to the mm-switch control plane.
pub const DEP_SWITCH: &str = "mm_switch";
/// Label used for calls to LNBits.
pub const DEP_LNBITS: &str = "lnbits";

/// Adds a metered alternative to `RequestBuilder::send`.
///
/// An extension trait rather than a free function so that instrumenting a call site is a
/// one-token edit — `.send()` becomes `.send_timed(DEP_SWITCH)` — and the long request
/// chains stay readable instead of being turned inside out.
pub trait SendTimed {
    /// Like `send()`, but records latency and outcome against `dependency`.
    fn send_timed(
        self,
        dependency: &'static str,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send;
}

impl SendTimed for reqwest::RequestBuilder {
    fn send_timed(
        self,
        dependency: &'static str,
    ) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + Send {
        send(dependency, self)
    }
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
        assert!(REQUEST_TIMEOUT.as_secs() > 0 && REQUEST_TIMEOUT.as_secs() <= 120);

        // The backstop must not be tighter than mm-switch's own 60s WriteTimeout, or it
        // converts slow-but-valid switch calls into client-side failures.
        assert!(
            REQUEST_TIMEOUT.as_secs() >= 60,
            "the shared timeout must not undercut mm-switch's declared 60s write budget"
        );
        assert!(
            CONNECT_TIMEOUT < REQUEST_TIMEOUT,
            "the connect budget must fit inside the overall request budget"
        );
    }
}
