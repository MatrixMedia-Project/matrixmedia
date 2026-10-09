//! What the fleet runner exposes on `/metrics` (spec §8.5). Registered by the runner only;
//! mm-core publishes its own fleet health gauges from the database (mm_core::metrics_global).
//! Label values are bounded: provider ids (a handful), zones, a fixed outcome set.

use std::sync::LazyLock;

use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

pub static RUNNER_HEARTBEAT_TIMESTAMP: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "mm_fleet_runner_heartbeat_timestamp",
        "Unix time of the runner's last successful heartbeat",
    )
    .expect("mm_fleet_runner_heartbeat_timestamp")
});

pub static PROVIDER_STATE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    IntGaugeVec::new(
        Opts::new(
            "mm_fleet_provider_state",
            "1 for each provider's current verdict",
        ),
        &["provider", "state"],
    )
    .expect("mm_fleet_provider_state")
});

pub static PROVIDER_CHECK_SECONDS: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::with_opts(HistogramOpts::new(
        "mm_fleet_provider_check_seconds",
        "How long one provider check took",
    ))
    .expect("mm_fleet_provider_check_seconds")
});

pub static CREATE_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "mm_fleet_create_total",
            "Provider create calls, by outcome (ok, capacity, quota, permanent, transient)",
        ),
        &["provider", "zone", "outcome"],
    )
    .expect("mm_fleet_create_total")
});

pub static GPU_NODES_RUNNING: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "mm_fleet_gpu_nodes_running",
        "GPU nodes the runner created that are not gone",
    )
    .expect("mm_fleet_gpu_nodes_running")
});

pub static ORPHANS_FOUND: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "mm_fleet_orphans_found_total",
            "Machines carrying the API fleet tag that no node row knows, by provider",
        ),
        &["provider"],
    )
    .expect("mm_fleet_orphans_found_total")
});

pub static REQUESTS_EXPIRED: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "mm_fleet_requests_expired_total",
        "Operator requests that expired unanswered",
    )
    .expect("mm_fleet_requests_expired_total")
});

pub fn register_runner(r: &Registry) -> prometheus::Result<()> {
    r.register(Box::new(RUNNER_HEARTBEAT_TIMESTAMP.clone()))?;
    r.register(Box::new(PROVIDER_STATE.clone()))?;
    r.register(Box::new(PROVIDER_CHECK_SECONDS.clone()))?;
    r.register(Box::new(CREATE_TOTAL.clone()))?;
    r.register(Box::new(GPU_NODES_RUNNING.clone()))?;
    r.register(Box::new(ORPHANS_FOUND.clone()))?;
    r.register(Box::new(REQUESTS_EXPIRED.clone()))?;
    Ok(())
}

/// Every outcome `mm_fleet_create_total` counts.
pub const CREATE_OUTCOMES: [&str; 5] = ["ok", "capacity", "quota", "permanent", "transient"];

/// Makes the `mm_fleet_create_total` series of one provider and zone exist, at 0, for every
/// outcome. `increase()` cannot see the first sample of a series, so a series born at 1 hides
/// the first create failure or refusal in that zone from the create alerts; one that was scraped
/// at 0 before the first create shows it. Called wherever a zone becomes one that may be
/// created in: each provider check, and before a create is sent.
pub fn register_create_series(provider_id: &str, zone: &str) {
    for outcome in CREATE_OUTCOMES {
        let _ = CREATE_TOTAL.with_label_values(&[provider_id, zone, outcome]);
    }
}

pub fn count_create(provider_id: &str, zone: &str, outcome: &str) {
    CREATE_TOTAL
        .with_label_values(&[provider_id, zone, outcome])
        .inc();
}
