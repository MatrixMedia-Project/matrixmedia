//! Process-wide collectors for call sites that cannot reach `AppState`.
//!
//! The rest of the metrics in [`crate::metrics::Metrics`] are owned by the API's
//! application state and reached through `State<SharedState>`. That works for handlers,
//! but three of the sites this module instruments cannot get there:
//!
//! * the shared outbound HTTP client ([`crate::http`]) is a process-wide static,
//! * the `AuthUser` extractor is generic over the state type and deliberately ignores it,
//! * the background-task supervisor runs before any router exists,
//! * the internal webhook routes are built from the config handle alone.
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

/// LiveKit webhooks that passed verification, by event type.
///
/// `event` is the LiveKit event name for the known types and `unknown` for anything
/// else (see `mm_sfu::webhook::WebhookEventType::as_str`), so the set stays bounded.
/// mm-core only observes these events; the counter shows which ones production
/// LiveKit actually sends.
pub static SFU_WEBHOOK_EVENTS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_sfu_webhook_events_total",
            "Verified LiveKit webhook events by event type"
        ),
        &["event"],
    )
    .expect("mm_sfu_webhook_events_total definition")
});

/// LiveKit webhook requests refused, by reason: `missing_auth` | `invalid_signature` |
/// `undecodable` | `not_configured`.
///
/// A steady `invalid_signature` rate after a deploy usually means LiveKit signs with a
/// different key than mm-core's `MM_SFU_LIVEKIT_API_KEY` (livekit.yaml `webhook.api_key`).
pub static SFU_WEBHOOK_REJECTED_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_sfu_webhook_rejected_total",
            "LiveKit webhook requests rejected, by reason"
        ),
        &["reason"],
    )
    .expect("mm_sfu_webhook_rejected_total definition")
});

/// Ad switches that could not be sent to the node holding the viewer (FR-405).
///
/// Any non-zero value is an ad that was charged for, or nearly charged for, without
/// being shown — mm-switch no-ops for a viewer it does not know, so the failure is
/// otherwise invisible. `reason` distinguishes a viewer who has since moved to a
/// different node from one whose node is gone entirely.
pub static AD_SWITCH_AFFINITY_MISMATCH: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_ad_switch_affinity_mismatch_total",
            "Ad switches not routable to the viewer's own node (reason: moved | node_gone | unplaced)"
        ),
        &["reason"],
    )
    .expect("mm_ad_switch_affinity_mismatch_total definition")
});

// ---------------------------------------------------------------------------
// Broadcast fleet (WS-A Task 7).
//
// These live here rather than on `Metrics` because the fleet runner and the
// deadline sweeper are background tasks that never see `AppState`.
// ---------------------------------------------------------------------------

/// Fleet size by shape and lifecycle position.
///
/// `ownership` is on the gauge on purpose: it is the difference between "we have
/// eight machines" and "we are paying by the hour for eight machines". All three
/// labels come from fixed, small sets (see `mm_core::fleet`).
pub static FLEET_NODES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    IntGaugeVec::new(
        opts!(
            "mm_fleet_nodes",
            "Fleet nodes by flavor, state and ownership"
        ),
        &["flavor", "state", "ownership"],
    )
    .expect("mm_fleet_nodes definition")
});

/// Time from writing a desired row to the node reporting healthy.
///
/// One of the four quantities the design leaves unmeasured, and the one the
/// planner most depends on: it is the window during which capacity is paid for
/// and unusable, and the reason the planner counts `Requested`/`Booting` nodes as
/// capacity rather than re-ordering them.
pub static FLEET_PROVISION_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
        HistogramOpts::new(
            "mm_fleet_provision_seconds",
            "Seconds from desired row to healthy, by flavor",
        )
        .buckets(vec![
            5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 180.0, 300.0, 600.0,
        ]),
        &["flavor"],
    )
    .expect("mm_fleet_provision_seconds definition")
});

/// Machines found at a provider that mm-core did not know it owned.
///
/// **Not a capacity metric.** Any non-zero value means the primary bookkeeping
/// path failed and the cheap safety net caught a machine we were paying for
/// silently. Alerted on `> 0`, not on a rate (NFR-704).
pub static FLEET_ORPHANS_DESTROYED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_fleet_orphans_destroyed_total",
            "Unknown provider instances destroyed by the orphan sweeper, by provider"
        ),
        &["provider"],
    )
    .expect("mm_fleet_orphans_destroyed_total definition")
});

/// Nodes destroyed because their `destroy_deadline` passed.
///
/// Also not a capacity metric. The deadline is the backstop: a node should be
/// torn down when its broadcast ends, and reaching the deadline means that did
/// not happen. Alerted on `> 0` (NFR-704).
pub static FLEET_REAPER_DEADLINE_KILLS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        opts!(
            "mm_fleet_reaper_deadline_kills_total",
            "Nodes destroyed by deadline rather than by normal teardown, by flavor"
        ),
        &["flavor"],
    )
    .expect("mm_fleet_reaper_deadline_kills_total definition")
});

/// Concurrent viewers by broadcaster tier.
///
/// **Deliberately NOT labelled by stream id**, which the plan asked for. A stream
/// id is user-generated and unbounded: every broadcast that has ever run would
/// leave a permanent time series behind, and a few thousand streams is enough to
/// make the whole endpoint the most expensive thing Prometheus scrapes. Per-stream
/// viewer counts already exist in `mm_stream_participants` and the operator
/// console reads them from there, which is also the only place they can be
/// queried after the retention window. `tier` is bounded and is what capacity
/// planning actually needs.
pub static BROADCAST_VIEWERS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    IntGaugeVec::new(
        opts!(
            "mm_broadcast_viewers",
            "Concurrent WebRTC viewers by broadcaster tier"
        ),
        &["tier"],
    )
    .expect("mm_broadcast_viewers definition")
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
    registry.register(Box::new(SFU_WEBHOOK_EVENTS_TOTAL.clone()))?;
    registry.register(Box::new(SFU_WEBHOOK_REJECTED_TOTAL.clone()))?;
    registry.register(Box::new(AD_SWITCH_AFFINITY_MISMATCH.clone()))?;
    registry.register(Box::new(FLEET_NODES.clone()))?;
    registry.register(Box::new(FLEET_PROVISION_SECONDS.clone()))?;
    registry.register(Box::new(FLEET_ORPHANS_DESTROYED.clone()))?;
    registry.register(Box::new(FLEET_REAPER_DEADLINE_KILLS.clone()))?;
    registry.register(Box::new(BROADCAST_VIEWERS.clone()))?;
    Ok(())
}

/// Republish `mm_fleet_nodes` from a complete observation of the fleet.
///
/// Resets first. A gauge vector keeps every label combination it has ever been
/// given, so without the reset the last node of a given shape leaves its count
/// frozen at 1 forever — and "one rented fan-out node exists" is exactly the
/// series an operator would trust when deciding whether we are still paying for
/// anything.
pub fn publish_fleet_nodes(nodes: &[crate::fleet::FleetNode]) {
    FLEET_NODES.reset();
    for n in nodes {
        FLEET_NODES
            .with_label_values(&[n.flavor.as_str(), n.state.as_str(), n.ownership.as_str()])
            .inc();
    }
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

    #[test]
    fn sfu_webhook_counters_are_exported() {
        let r = Registry::new();
        register_all(&r).expect("register");
        SFU_WEBHOOK_EVENTS_TOTAL
            .with_label_values(&["egress_ended"])
            .inc();
        SFU_WEBHOOK_REJECTED_TOTAL
            .with_label_values(&["invalid_signature"])
            .inc();

        let names: Vec<String> = r.gather().iter().map(|f| f.get_name().to_owned()).collect();
        assert!(
            names.iter().any(|n| n == "mm_sfu_webhook_events_total"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|n| n == "mm_sfu_webhook_rejected_total"),
            "{names:?}"
        );
    }
}

#[cfg(test)]
mod fleet_metric_tests {
    use super::*;
    use crate::fleet::{FleetNode, NodeFlavor, NodeId, NodeState, Ownership};

    /// These collectors are process-global statics and `cargo test` runs tests in
    /// parallel threads of ONE process, so two tests publishing `FLEET_NODES`
    /// race and fail each other intermittently. Serialise the ones that mutate
    /// shared collectors.
    fn metric_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn node(id: &str, flavor: NodeFlavor, state: NodeState, ownership: Ownership) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor,
            ownership,
            state,
            viewer_capacity: 250,
            viewers_current: 0,
        }
    }

    /// Every fleet series must be registered, or the NFR-704 alerts select
    /// nothing and can never fire — the same class of bug the alert file's own
    /// comments record twice (a `job=` selector matching no series, and a counter
    /// nothing increments).
    #[test]
    fn every_fleet_collector_registers_into_a_fresh_registry() {
        let _guard = metric_lock();
        let registry = Registry::new();
        register_all(&registry).expect("fleet collectors must register");

        // Give each series a value. A registered collector with no children emits
        // no family at all, so a scrape-based assertion needs an observation.
        publish_fleet_nodes(&[node(
            "n1",
            NodeFlavor::Fanout,
            NodeState::Healthy,
            Ownership::Rented,
        )]);
        FLEET_PROVISION_SECONDS.with_label_values(&["fanout"]).observe(42.0);
        FLEET_ORPHANS_DESTROYED.with_label_values(&["itldc"]).inc();
        FLEET_REAPER_DEADLINE_KILLS.with_label_values(&["fanout"]).inc();
        BROADCAST_VIEWERS.with_label_values(&["verified"]).set(7);
        AD_SWITCH_AFFINITY_MISMATCH.with_label_values(&["moved"]).inc();

        let families = registry.gather();
        let names: Vec<&str> = families.iter().map(|f| f.get_name()).collect();
        for expected in [
            "mm_fleet_nodes",
            "mm_fleet_provision_seconds",
            "mm_fleet_orphans_destroyed_total",
            "mm_fleet_reaper_deadline_kills_total",
            "mm_broadcast_viewers",
            "mm_ad_switch_affinity_mismatch_total",
        ] {
            assert!(
                names.contains(&expected),
                "missing metric: {expected} (registered: {names:?})"
            );
        }
    }

    /// The reset is the whole point. Without it the last node of a shape leaves
    /// its count stuck at 1, and an operator reading "one rented node exists"
    /// would keep looking for a machine that was destroyed hours ago.
    #[test]
    fn a_fleet_that_shrinks_to_nothing_reports_nothing() {
        let _guard = metric_lock();
        publish_fleet_nodes(&[
            node("n1", NodeFlavor::Fanout, NodeState::Healthy, Ownership::Rented),
            node("n2", NodeFlavor::Fanout, NodeState::Healthy, Ownership::Rented),
        ]);
        assert_eq!(
            FLEET_NODES
                .with_label_values(&["fanout", "healthy", "rented"])
                .get(),
            2
        );

        publish_fleet_nodes(&[]);
        assert_eq!(
            FLEET_NODES
                .with_label_values(&["fanout", "healthy", "rented"])
                .get(),
            0,
            "a destroyed node must stop being counted"
        );
    }

    /// Cardinality guard. `mm_broadcast_viewers` must never be labelled by stream
    /// id: it is user-generated and unbounded, so every broadcast that ever ran
    /// would leave a permanent series behind.
    #[test]
    fn broadcast_viewers_is_labelled_by_tier_only() {
        let _guard = metric_lock();
        let registry = Registry::new();
        register_all(&registry).expect("register");
        BROADCAST_VIEWERS.with_label_values(&["open"]).set(1);

        let family = registry
            .gather()
            .into_iter()
            .find(|f| f.get_name() == "mm_broadcast_viewers")
            .expect("mm_broadcast_viewers must be registered");

        for metric in family.get_metric() {
            let labels: Vec<&str> = metric.get_label().iter().map(|l| l.get_name()).collect();
            assert_eq!(
                labels,
                vec!["tier"],
                "unexpected labels {labels:?} — a stream id here is a cardinality bomb"
            );
        }
    }
}
