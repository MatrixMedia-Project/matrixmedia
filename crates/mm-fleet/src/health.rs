//! What mm-core watches about the fleet runner, from the database (spec D-C8, §9). The
//! alerts that must fire while the runner is down cannot come from the runner.
//!
//! Read-only and cheap: one statement over indexed, bounded tables, no provider call, nothing
//! secret read or exposed (counts and ages only, no labels).

use sqlx::PgPool;

use crate::placement::CHECK_FRESH_SECS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FleetHealth {
    /// Seconds since the runner's last heartbeat, by the database's clock; -1 when it has never
    /// heartbeat. Never negative otherwise: a heartbeat ahead of the clock reads 0.
    pub heartbeat_age_secs: i64,
    /// Rented machines not yet gone, whoever made them (the API path or Terraform).
    pub rented_live: i64,
    /// The largest overrun past `destroy_deadline` among rented, not-gone machines the runner
    /// made through a provider API; 0 when none is past its deadline. Machines made through
    /// Terraform are left out (none are recorded today): they are not the API path's, so they
    /// must not page as runner overruns.
    pub max_overrun_secs: i64,
    /// Enabled providers whose token or endpoint only a human can fix (`needs_you`,
    /// `endpoint_mismatch`).
    pub providers_need_attention: i64,
    /// Enabled providers with a token whose verdict placement does not accept, other than the
    /// ones counted above. Placement accepts an `ok` verdict checked at or after the token was
    /// entered and no older than [`CHECK_FRESH_SECS`] (`placement::verified`); a test keeps this
    /// count and that rule in step. The two provider counts never overlap, so each alert says
    /// one thing.
    ///
    /// A provider whose kind has no checks yet is left out: the runner records it as
    /// `unknown` with error kind `unsupported` on every pass (the dashboard shows "Checks not
    /// built yet"), so it can never become verified and would count for as long as it is
    /// enabled. Nothing is wrong with it, so nothing should page.
    pub providers_unverified: i64,
}

/// One statement, so every figure comes from the same snapshot and the same `now()`.
const SAMPLE: &str = "
SELECT
  (SELECT GREATEST(EXTRACT(EPOCH FROM now() - heartbeat_at), 0)::BIGINT
     FROM mm_fleet_control WHERE id = 1) AS heartbeat_age,
  (SELECT count(*) FROM mm_fleet_nodes
    WHERE ownership = 'rented' AND state <> 'gone') AS rented_live,
  (SELECT coalesce(max(EXTRACT(EPOCH FROM now() - destroy_deadline))::BIGINT, 0)
     FROM mm_fleet_nodes
    WHERE ownership = 'rented' AND created_backend = 'api' AND state <> 'gone'
      AND destroy_deadline < now()) AS max_overrun,
  (SELECT count(*) FROM mm_fleet_providers p
     JOIN mm_fleet_provider_status s ON s.provider_id = p.id
    WHERE p.deleted_at IS NULL AND p.enabled
      AND s.state IN ('needs_you', 'endpoint_mismatch')) AS need_attention,
  (SELECT count(*) FROM mm_fleet_providers p
     JOIN mm_fleet_provider_credentials c ON c.provider_id = p.id
     LEFT JOIN mm_fleet_provider_status s ON s.provider_id = p.id
    WHERE p.deleted_at IS NULL AND p.enabled
      AND coalesce(s.state, '') NOT IN ('needs_you', 'endpoint_mismatch')
      AND coalesce(s.last_error_kind, '') <> 'unsupported'
      AND NOT coalesce(
            s.state = 'ok'
            AND s.checked_at >= c.entered_at
            AND s.checked_at >= now() - make_interval(secs => $1),
            false)) AS unverified";

pub async fn sample(pool: &PgPool) -> sqlx::Result<FleetHealth> {
    let (age, rented_live, max_overrun_secs, providers_need_attention, providers_unverified): (
        Option<i64>,
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(SAMPLE)
        .bind(CHECK_FRESH_SECS as f64)
        .fetch_one(pool)
        .await?;
    Ok(FleetHealth {
        heartbeat_age_secs: age.unwrap_or(-1),
        rented_live,
        max_overrun_secs,
        providers_need_attention,
        providers_unverified,
    })
}

pub fn publish(h: &FleetHealth) {
    use mm_core::metrics_global as g;
    g::FLEET_RUNNER_HEARTBEAT_AGE.set(h.heartbeat_age_secs);
    g::FLEET_RENTED_NODES_LIVE.set(h.rented_live);
    g::FLEET_NODE_OVERRUN.set(h.max_overrun_secs);
    g::FLEET_PROVIDERS_NEED_ATTENTION.set(h.providers_need_attention);
    g::FLEET_PROVIDERS_UNVERIFIED.set(h.providers_unverified);
}
