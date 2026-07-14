//! Process-wide collectors for call sites that cannot reach `AppState`.
//!
//! The rest of the metrics in [`crate::metrics::Metrics`] are owned by the API's
//! application state and reached through `State<SharedState>`. That works for handlers,
//! but three of the sites this module instruments cannot get there:
//!
//! * the shared outbound HTTP client ([`crate::http`]) is a process-wide static,
//! * the `AuthUser` extractor is generic over the state type and deliberately ignores it,
//! * the background-task supervisor runs before any router exists.
//!
//! Rather than refactor state into all three, these collectors live here as statics and
//! are registered into whatever `Registry` a `Metrics` builds. Prometheus collectors are
//! `Arc`-backed and cheap to clone, and a `Registry` only rejects duplicates *within
//! itself*, so the same collector can be handed to several registries (as the tests do)
//! without conflict.
//!
//! **Cardinality is deliberately bounded.** Every label below takes values from a fixed,
//! small set: route labels come from the matched *path template* (`/streams/{id}`), never
//! the raw URI, and dependency/outcome/stage labels are `&'static str` chosen at the call
//! site. Nothing user-controlled ever becomes a label value.

use std::sync::LazyLock;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Registry, opts};

/// Buckets for inbound HTTP handler latency, in seconds.
///
/// Weighted toward the fast end: a healthy MM request is single-digit milliseconds, and
/// the interesting question is "how far into the tail did we go", not "was it 4s or 5s".
const HTTP_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Buckets for outbound-dependency latency, in seconds.
///
/// Runs further out than the inbound buckets: these calls cross the network, and the
/// point of the series is to catch a dependency sliding toward the 30s request timeout
/// enforced by [`crate::http::shared`].
const OUTBOUND_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Inbound HTTP latency, by matched route template, method and status class.
pub static HTTP_REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "mm_http_request_duration_seconds",
            "Inbound HTTP request latency by matched route, method and status",
        )
        .buckets(HTTP_BUCKETS.to_vec()),
        &["route", "method", "status"],
    )
    .expect("mm_http_request_duration_seconds definition")
});

/// Outbound dependency latency, by dependency and outcome.
///
/// `dependency` is one of a fixed set (`synapse`, `mm_switch`, `lnbits`); `outcome` is
/// `ok` | `http_error` | `timeout` | `transport_error`.
pub static OUTBOUND_REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "mm_outbound_request_duration_seconds",
            "Outbound dependency request latency by dependency and outcome",
        )
        .buckets(OUTBOUND_BUCKETS.to_vec()),
        &["dependency", "outcome"],
    )
    .expect("mm_outbound_request_duration_seconds definition")
});

/// Auth validations split by which stage settled the request.
///
/// `stage` is `jwt` (local HS256 verify, no network) or `matrix_bearer` (whoami against
/// Synapse). The existing `mm_auth_validations_total` counts both together, which hides
/// exactly the thing worth knowing: how much of MM's auth traffic Synapse is absorbing.
pub static AUTH_STAGE_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_auth_stage_total",
            "Auth validations by stage (jwt | matrix_bearer) and outcome"
        ),
        &["stage", "outcome"],
    )
    .expect("mm_auth_stage_total definition")
});

/// Whoami cache outcomes (`hit` | `miss`).
///
/// The hit ratio is the acceptance signal for the whoami cache: a low ratio means the
/// cache is not doing its job and Synapse is still absorbing MM's auth rate.
pub static WHOAMI_CACHE_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_whoami_cache_total",
            "Matrix whoami cache lookups by result (hit | miss)"
        ),
        &["result"],
    )
    .expect("mm_whoami_cache_total definition")
});

/// Unix timestamp of each supervised background task's last completed iteration.
///
/// A gauge of "when did this last make progress" rather than a liveness boolean: a task
/// that is *running* but wedged looks identical to a healthy one under a boolean, and
/// wedged is the failure mode that actually happens. Alert on `time() - heartbeat`.
pub static BACKGROUND_TASK_HEARTBEAT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    IntGaugeVec::new(
        opts!(
            "mm_background_task_heartbeat_timestamp_seconds",
            "Unix timestamp of a background task's last completed iteration"
        ),
        &["task"],
    )
    .expect("mm_background_task_heartbeat_timestamp_seconds definition")
});

/// Times the supervisor restarted a background task (panic or unexpected return).
///
/// Before the supervisor existed a panicking task simply vanished and the server carried
/// on looking healthy. Any non-zero value here is a bug worth chasing.
pub static BACKGROUND_TASK_RESTARTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_background_task_restarts_total",
            "Background task restarts by task and reason (panic | returned)"
        ),
        &["task", "reason"],
    )
    .expect("mm_background_task_restarts_total definition")
});

/// Register every global collector into `registry`.
///
/// Called by [`crate::metrics::Metrics::new`] so the `/metrics` endpoint exposes these
/// alongside the state-owned metrics.
pub fn register_all(registry: &Registry) -> prometheus::Result<()> {
    registry.register(Box::new(HTTP_REQUEST_DURATION.clone()))?;
    registry.register(Box::new(OUTBOUND_REQUEST_DURATION.clone()))?;
    registry.register(Box::new(AUTH_STAGE_TOTAL.clone()))?;
    registry.register(Box::new(WHOAMI_CACHE_TOTAL.clone()))?;
    registry.register(Box::new(BACKGROUND_TASK_HEARTBEAT.clone()))?;
    registry.register(Box::new(BACKGROUND_TASK_RESTARTS.clone()))?;
    Ok(())
}

/// Record that `task` just finished an iteration.
pub fn heartbeat(task: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    BACKGROUND_TASK_HEARTBEAT
        .with_label_values(&[task])
        .set(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole design rests on this: several `Metrics` instances exist in one process
    /// (every integration test builds one), and each must be able to adopt the same
    /// global collectors without a duplicate-registration error.
    #[test]
    fn the_same_collectors_register_into_several_registries() {
        let a = Registry::new();
        let b = Registry::new();
        register_all(&a).expect("first registry");
        register_all(&b).expect("second registry");

        assert!(!a.gather().is_empty());
        assert!(!b.gather().is_empty());
    }

    /// Registering twice into the *same* registry is an error — this is what would bite
    /// if `register_all` were ever called from two places on one registry.
    #[test]
    fn a_single_registry_rejects_a_duplicate() {
        let r = Registry::new();
        register_all(&r).expect("first");
        assert!(register_all(&r).is_err(), "duplicate must be rejected");
    }

    #[test]
    fn heartbeat_records_a_plausible_timestamp() {
        heartbeat("unit_test_task");
        let v = BACKGROUND_TASK_HEARTBEAT
            .with_label_values(&["unit_test_task"])
            .get();
        // Any time after 2020 — proves we wrote a wall-clock value, not 0 or an uptime.
        assert!(v > 1_577_836_800, "heartbeat should be a unix timestamp");
    }

    #[test]
    fn label_sets_are_what_the_alert_rules_expect() {
        // The shipped alert rules select on these exact labels; a rename here silently
        // turns every alert into one that can never fire.
        HTTP_REQUEST_DURATION
            .with_label_values(&["/streams/{id}", "GET", "200"])
            .observe(0.01);
        OUTBOUND_REQUEST_DURATION
            .with_label_values(&["synapse", "ok"])
            .observe(0.01);
        AUTH_STAGE_TOTAL
            .with_label_values(&["matrix_bearer", "ok"])
            .inc();
        WHOAMI_CACHE_TOTAL.with_label_values(&["hit"]).inc();
        BACKGROUND_TASK_RESTARTS
            .with_label_values(&["stream_sweep", "panic"])
            .inc();
    }
}
