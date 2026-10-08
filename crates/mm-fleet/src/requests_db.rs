//! Operator requests the runner answers (spec §6.2 "Requests"). A row is a delayed button
//! press: it expires unclaimed after REQUEST_TTL_SECS so a runner returning from an outage
//! never runs a click from hours ago.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};

pub const REQUEST_TTL_SECS: i64 = 300;

/// A running test boot spans create, boot, report, destroy and the absence check; its claim
/// is released as a dead runner's only after this long. Test connection keeps REQUEST_TTL_SECS.
pub const TEST_BOOT_RUNNING_TTL_SECS: i64 = 1500;

/// SQL for "this running row's claim is still alive", per kind. Every query that asks whether
/// a runner may still be working on a request uses this one text.
fn claim_alive() -> String {
    format!(
        "claimed_at > now() - make_interval(secs => CASE kind
             WHEN 'test_boot' THEN {TEST_BOOT_RUNNING_TTL_SECS} ELSE {REQUEST_TTL_SECS} END)"
    )
}

#[derive(Debug, Clone)]
pub struct NewRequest<'a> {
    pub kind: &'a str,
    pub provider_id: &'a str,
    pub zone: Option<&'a str>,
    pub role: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub requested_by: &'a str,
    /// What mm-core fixes when it queues the request (a test boot's report URL); `{}` for a
    /// request that needs none. The admin API serves it with the rest of the row, so it is
    /// never a secret.
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestRow {
    pub id: String,
    pub kind: String,
    pub provider_id: String,
    pub zone: Option<String>,
    pub role: Option<String>,
    pub reason: Option<String>,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub state: String,
    pub result: Option<Value>,
    /// See [`NewRequest::params`].
    pub params: Value,
}

const COLS: &str = "id, kind, provider_id, zone, role, reason, requested_by, requested_at, expires_at, claimed_at, finished_at, state, result, params";

fn from_row(r: &sqlx::postgres::PgRow) -> RequestRow {
    RequestRow {
        id: r.get("id"),
        kind: r.get("kind"),
        provider_id: r.get("provider_id"),
        zone: r.get("zone"),
        role: r.get("role"),
        reason: r.get("reason"),
        requested_by: r.get("requested_by"),
        requested_at: r.get("requested_at"),
        expires_at: r.get("expires_at"),
        claimed_at: r.get("claimed_at"),
        finished_at: r.get("finished_at"),
        state: r.get("state"),
        result: r.get("result"),
        params: r.get("params"),
    }
}

pub async fn enqueue(pool: &PgPool, n: &NewRequest<'_>) -> sqlx::Result<String> {
    let id = format!("r-{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mm_fleet_requests (id, kind, provider_id, zone, role, reason, requested_by, expires_at, params)
         VALUES ($1,$2,$3,$4,$5,$6,$7, now() + make_interval(secs => $8), $9)",
    )
    .bind(&id)
    .bind(n.kind)
    .bind(n.provider_id)
    .bind(n.zone)
    .bind(n.role)
    .bind(n.reason)
    .bind(n.requested_by)
    .bind(REQUEST_TTL_SECS as f64)
    .bind(&n.params)
    .execute(pool)
    .await?;
    Ok(id)
}

/// One statement: the subquery locks the oldest live queued row of `kind` with SKIP LOCKED, so
/// two runners (or one runner and a stuck transaction) never claim the same request, and a
/// loop that answers one kind never takes another's. A row past its `expires_at` is never
/// claimable, even before `expire_stale` has marked it.
pub async fn claim_next(pool: &PgPool, kind: &str) -> sqlx::Result<Option<RequestRow>> {
    Ok(sqlx::query(&format!(
        "UPDATE mm_fleet_requests SET state = 'running', claimed_at = now()
         WHERE id = (SELECT id FROM mm_fleet_requests
                      WHERE state = 'queued' AND kind = $1 AND expires_at > now()
                      ORDER BY requested_at, id LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING {COLS}"
    ))
    .bind(kind)
    .fetch_optional(pool)
    .await?
    .map(|r| from_row(&r)))
}

/// Marks as `expired` (a) queued rows past their `expires_at` and (b) running rows whose claim
/// is no longer alive (REQUEST_TTL_SECS, or TEST_BOOT_RUNNING_TTL_SECS for a test boot): a
/// runner that has not finished within that time is dead; its claim is released as expired.
/// Returns the total number of rows expired.
pub async fn expire_stale(pool: &PgPool) -> sqlx::Result<u64> {
    let queued = sqlx::query(
        "UPDATE mm_fleet_requests SET state = 'expired', finished_at = now() WHERE state = 'queued' AND expires_at <= now()",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let alive = claim_alive();
    let running = sqlx::query(&format!(
        "UPDATE mm_fleet_requests SET state = 'expired', finished_at = now()
         WHERE state = 'running' AND NOT ({alive})"
    ))
    .execute(pool)
    .await?
    .rows_affected();
    Ok(queued + running)
}

/// Merges `patch` into a running request's result (phase, node, timings). `false` when the
/// request is not running any more.
pub async fn progress(pool: &PgPool, id: &str, patch: Value) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "UPDATE mm_fleet_requests SET result = coalesce(result, '{}'::jsonb) || $2
          WHERE id = $1 AND state = 'running'",
    )
    .bind(id)
    .bind(patch)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Finishes a running request. `result` is merged into whatever `progress` recorded.
pub async fn finish(pool: &PgPool, id: &str, ok: bool, result: Value) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE mm_fleet_requests
            SET state = $2, finished_at = now(), result = coalesce(result, '{}'::jsonb) || $3
          WHERE id = $1 AND state = 'running'",
    )
    .bind(id)
    .bind(if ok { "done" } else { "failed" })
    .bind(result)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, id: &str) -> sqlx::Result<Option<RequestRow>> {
    Ok(sqlx::query(&format!(
        "SELECT {COLS} FROM mm_fleet_requests WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .map(|r| from_row(&r)))
}

/// The requests of a kind a runner has claimed and not finished, oldest first.
pub async fn running(pool: &PgPool, kind: &str) -> sqlx::Result<Vec<RequestRow>> {
    Ok(sqlx::query(&format!(
        "SELECT {COLS} FROM mm_fleet_requests WHERE kind = $1 AND state = 'running' ORDER BY requested_at, id"
    ))
    .bind(kind)
    .fetch_all(pool)
    .await?
    .iter()
    .map(from_row)
    .collect())
}

/// Requests of a kind still live anywhere: queued and unexpired, or running with a live claim.
/// Takes any executor, so a caller can count inside a transaction that holds a lock.
pub async fn live_count<'e>(db: impl sqlx::PgExecutor<'e>, kind: &str) -> sqlx::Result<i64> {
    let alive = claim_alive();
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM mm_fleet_requests WHERE kind = $1
           AND ((state = 'queued' AND expires_at > now()) OR (state = 'running' AND {alive}))"
    ))
    .bind(kind)
    .fetch_one(db)
    .await
}

/// Requests of a kind made since midnight UTC, whatever became of them. Takes any executor,
/// like [`live_count`].
pub async fn count_today<'e>(db: impl sqlx::PgExecutor<'e>, kind: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_requests
          WHERE kind = $1 AND requested_at >= (date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC')",
    )
    .bind(kind)
    .fetch_one(db)
    .await
}

/// Refuses every queued request of a kind at once (`fleet.mode = off`). A running request is
/// the runner's to finish.
pub async fn fail_queued(pool: &PgPool, kind: &str, reason: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE mm_fleet_requests
            SET state = 'failed', claimed_at = now(), finished_at = now(), result = jsonb_build_object('error', $2::text)
          WHERE kind = $1 AND state = 'queued'",
    )
    .bind(kind)
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Live requests only: queued and not yet expired, or running with a live claim.
/// A row that `expire_stale` has not yet swept (e.g. a dead runner's claim) does not count.
pub async fn count_queued_for(pool: &PgPool, provider_id: &str, kind: &str) -> sqlx::Result<i64> {
    let alive = claim_alive();
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM mm_fleet_requests WHERE provider_id = $1 AND kind = $2
         AND ((state = 'queued' AND expires_at > now()) OR (state = 'running' AND {alive}))"
    ))
    .bind(provider_id)
    .bind(kind)
    .fetch_one(pool)
    .await
}
