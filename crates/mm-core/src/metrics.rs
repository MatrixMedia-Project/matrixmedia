use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntGauge, Registry, opts,
    register_histogram_with_registry, register_int_counter_with_registry,
    register_int_gauge_with_registry,
};

/// Application-wide Prometheus metrics.
///
/// All metric names use the `mm_` prefix.  Counters use the `_total` suffix,
/// gauges reflect current values, and histograms use seconds.
pub struct Metrics {
    /// The Prometheus registry that owns all metrics.
    pub registry: Registry,

    // -- Streams ---------------------------------------------------------------
    /// Number of currently active streams.
    pub streams_active: IntGauge,
    /// Total streams created since server start.
    pub streams_created_total: IntCounter,
    /// Total streams ended since server start.
    pub streams_ended_total: IntCounter,

    // -- Participants ----------------------------------------------------------
    /// Current number of connected participants across all streams.
    pub participant_count: IntGauge,
    /// Total participant join events.
    pub participant_joins_total: IntCounter,
    /// Total participant leave events.
    pub participant_leaves_total: IntCounter,

    // -- Latency ---------------------------------------------------------------
    /// Histogram of stream join latency in seconds.
    pub join_latency_seconds: Histogram,

    // -- HTTP ------------------------------------------------------------------
    /// Total HTTP requests handled.
    pub http_requests_total: IntCounter,

    // -- SFU health ------------------------------------------------------------
    /// SFU health: 1 = healthy, 0 = unhealthy.
    pub sfu_health_status: IntGauge,
    /// SFU circuit breaker state: 0 = closed, 1 = half-open, 2 = open.
    pub sfu_circuit_state: IntGauge,

    // -- Auth ------------------------------------------------------------------
    /// Total successful auth validations.
    pub auth_validations_total: IntCounter,
    /// Total auth validation failures.
    pub auth_failures_total: IntCounter,

    // -- Rate limiting ---------------------------------------------------------
    /// Total requests rejected by the rate limiter.
    pub rate_limit_rejected_total: IntCounter,

    // -- E2EE ------------------------------------------------------------------
    /// Current number of streams with E2EE enabled.
    pub streams_e2ee_active: IntGauge,
    /// Total E2EE key rotations triggered.
    pub e2ee_key_rotations_total: IntCounter,
    /// Total E2EE key distributions (initial + rotation publishes).
    pub e2ee_key_distributions_total: IntCounter,

    // -- Federation ------------------------------------------------------------
    /// Total successful federated OpenID validations.
    pub federation_validations_total: IntCounter,
    /// Total federated requests rejected by the allow/deny list.
    pub federation_rejections_total: IntCounter,
    /// Total federated OpenID validation errors (network/parse failures).
    pub federation_validation_errors_total: IntCounter,
    /// Total cross-server stream joins (user lives on a different homeserver).
    pub federated_joins_total: IntCounter,
}

impl Metrics {
    /// Create and register all metrics with a new [`Registry`].
    pub fn new() -> Self {
        let registry = Registry::new();

        let streams_active = register_int_gauge_with_registry!(
            opts!("mm_streams_active", "Number of currently active streams"),
            registry
        )
        .expect("mm_streams_active registration");

        let streams_created_total = register_int_counter_with_registry!(
            opts!(
                "mm_streams_created_total",
                "Total streams created since server start"
            ),
            registry
        )
        .expect("mm_streams_created_total registration");

        let streams_ended_total = register_int_counter_with_registry!(
            opts!(
                "mm_streams_ended_total",
                "Total streams ended since server start"
            ),
            registry
        )
        .expect("mm_streams_ended_total registration");

        let participant_count = register_int_gauge_with_registry!(
            opts!(
                "mm_participant_count",
                "Current number of connected participants"
            ),
            registry
        )
        .expect("mm_participant_count registration");

        let participant_joins_total = register_int_counter_with_registry!(
            opts!(
                "mm_participant_joins_total",
                "Total participant join events"
            ),
            registry
        )
        .expect("mm_participant_joins_total registration");

        let participant_leaves_total = register_int_counter_with_registry!(
            opts!(
                "mm_participant_leaves_total",
                "Total participant leave events"
            ),
            registry
        )
        .expect("mm_participant_leaves_total registration");

        let join_latency_seconds = register_histogram_with_registry!(
            HistogramOpts::new(
                "mm_join_latency_seconds",
                "Histogram of stream join latency in seconds"
            ),
            registry
        )
        .expect("mm_join_latency_seconds registration");

        let http_requests_total = register_int_counter_with_registry!(
            opts!("mm_http_requests_total", "Total HTTP requests handled"),
            registry
        )
        .expect("mm_http_requests_total registration");

        let sfu_health_status = register_int_gauge_with_registry!(
            opts!(
                "mm_sfu_health_status",
                "SFU health: 1 = healthy, 0 = unhealthy"
            ),
            registry
        )
        .expect("mm_sfu_health_status registration");

        let sfu_circuit_state = register_int_gauge_with_registry!(
            opts!(
                "mm_sfu_circuit_state",
                "SFU circuit breaker state: 0 = closed, 1 = half-open, 2 = open"
            ),
            registry
        )
        .expect("mm_sfu_circuit_state registration");

        let auth_validations_total = register_int_counter_with_registry!(
            opts!(
                "mm_auth_validations_total",
                "Total successful auth validations"
            ),
            registry
        )
        .expect("mm_auth_validations_total registration");

        let auth_failures_total = register_int_counter_with_registry!(
            opts!("mm_auth_failures_total", "Total auth validation failures"),
            registry
        )
        .expect("mm_auth_failures_total registration");

        let rate_limit_rejected_total = register_int_counter_with_registry!(
            opts!(
                "mm_rate_limit_rejected_total",
                "Total requests rejected by the rate limiter"
            ),
            registry
        )
        .expect("mm_rate_limit_rejected_total registration");

        let streams_e2ee_active = register_int_gauge_with_registry!(
            opts!(
                "mm_streams_e2ee_active",
                "Current number of streams with E2EE enabled"
            ),
            registry
        )
        .expect("mm_streams_e2ee_active registration");

        let e2ee_key_rotations_total = register_int_counter_with_registry!(
            opts!(
                "mm_e2ee_key_rotations_total",
                "Total E2EE key rotations triggered"
            ),
            registry
        )
        .expect("mm_e2ee_key_rotations_total registration");

        let e2ee_key_distributions_total = register_int_counter_with_registry!(
            opts!(
                "mm_e2ee_key_distributions_total",
                "Total E2EE key distributions (initial + rotation publishes)"
            ),
            registry
        )
        .expect("mm_e2ee_key_distributions_total registration");

        let federation_validations_total = register_int_counter_with_registry!(
            opts!(
                "mm_federation_validations_total",
                "Total successful federated OpenID validations"
            ),
            registry
        )
        .expect("mm_federation_validations_total registration");

        let federation_rejections_total = register_int_counter_with_registry!(
            opts!(
                "mm_federation_rejections_total",
                "Total federated requests rejected by the allow/deny list"
            ),
            registry
        )
        .expect("mm_federation_rejections_total registration");

        let federation_validation_errors_total = register_int_counter_with_registry!(
            opts!(
                "mm_federation_validation_errors_total",
                "Total federated OpenID validation errors (network/parse failures)"
            ),
            registry
        )
        .expect("mm_federation_validation_errors_total registration");

        let federated_joins_total = register_int_counter_with_registry!(
            opts!(
                "mm_federated_joins_total",
                "Total stream joins by federated users"
            ),
            registry
        )
        .expect("mm_federated_joins_total registration");

        Self {
            registry,
            streams_active,
            streams_created_total,
            streams_ended_total,
            participant_count,
            participant_joins_total,
            participant_leaves_total,
            join_latency_seconds,
            http_requests_total,
            sfu_health_status,
            sfu_circuit_state,
            auth_validations_total,
            auth_failures_total,
            rate_limit_rejected_total,
            streams_e2ee_active,
            e2ee_key_rotations_total,
            e2ee_key_distributions_total,
            federation_validations_total,
            federation_rejections_total,
            federation_validation_errors_total,
            federated_joins_total,
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_without_panic() {
        let m = Metrics::new();
        m.streams_created_total.inc();
        m.streams_active.set(1);
        m.participant_count.inc();
        m.join_latency_seconds.observe(0.05);

        let families = m.registry.gather();
        assert!(
            !families.is_empty(),
            "registry should contain registered metrics"
        );
    }
}
