//! Operator requests the runner answers (spec §6.2 "Requests"). A row is a delayed button
//! press: it expires unclaimed after REQUEST_TTL_SECS so a runner returning from an outage
//! never runs a click from hours ago.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};

pub const REQUEST_TTL_SECS: i64 = 300;

#[derive(Debug, Clone)]
pub struct NewRequest<'a> {
    pub kind: &'a str,
    pub provider_id: &'a str,
    pub zone: Option<&'a str>,
    pub role: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub requested_by: &'a str,
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
}

const COLS: &str = "id, kind, provider_id, zone, role, reason, requested_by, requested_at, expires_at, claimed_at, finished_at, state, result";

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
    }
}

pub async fn enqueue(pool: &PgPool, n: &NewRequest<'_>) -> sqlx::Result<String> {
    let id = format!("r-{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mm_fleet_requests (id, kind, provider_id, zone, role, reason, requested_by, expires_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7, now() + make_interval(secs => $8))",
    )
    .bind(&id)
    .bind(n.kind)
    .bind(n.provider_id)
    .bind(n.zone)
    .bind(n.role)
    .bind(n.reason)
    .bind(n.requested_by)
    .bind(REQUEST_TTL_SECS as f64)
    .execute(pool)
    .await?;
    Ok(id)
}

/// One statement: the subquery locks the oldest live queued row with SKIP LOCKED, so two
/// runners (or one runner and a stuck transaction) never claim the same request. A row past
/// its `expires_at` is never claimable, even before `expire_stale` has marked it.
pub async fn claim_next(pool: &PgPool) -> sqlx::Result<Option<RequestRow>> {
    Ok(sqlx::query(&format!(
        "UPDATE mm_fleet_requests SET state = 'running', claimed_at = now()
         WHERE id = (SELECT id FROM mm_fleet_requests WHERE state = 'queued' AND expires_at > now()
                     ORDER BY requested_at LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING {COLS}"
    ))
    .fetch_optional(pool)
    .await?
    .map(|r| from_row(&r)))
}

pub async fn expire_stale(pool: &PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE mm_fleet_requests SET state = 'expired', finished_at = now() WHERE state = 'queued' AND expires_at <= now()",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn finish(pool: &PgPool, id: &str, ok: bool, result: Value) -> sqlx::Result<()> {
    sqlx::query("UPDATE mm_fleet_requests SET state = $2, finished_at = now(), result = $3 WHERE id = $1 AND state = 'running'")
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

pub async fn count_queued_for(pool: &PgPool, provider_id: &str, kind: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM mm_fleet_requests WHERE provider_id = $1 AND kind = $2 AND state IN ('queued','running')")
        .bind(provider_id)
        .bind(kind)
        .fetch_one(pool)
        .await
}
