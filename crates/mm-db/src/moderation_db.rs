//! Raw-PgPool data access for content moderation (E3).
//!
//! Backed by `signup_pool` (the always-present `PgPool` on `AppState`),
//! matching the modern feature-module pattern (signups/feed_db/announcements).
//! Postgres-only; the moderation tables are never created in SQLite.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ModerationReport {
    pub id: Uuid,
    pub source: String,
    pub target_type: String,
    pub target_id: String,
    pub room_id: Option<String>,
    pub reported_user_id: Option<String>,
    pub reporter_id: Option<String>,
    pub reason: String,
    pub details: Option<String>,
    pub status: String,
    pub synapse_report_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved_by: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
}

pub struct NewReport<'a> {
    pub source: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub room_id: Option<&'a str>,
    pub reported_user_id: Option<&'a str>,
    pub reporter_id: Option<&'a str>,
    pub reason: &'a str,
    pub details: Option<&'a str>,
    pub synapse_report_id: Option<i64>,
}

/// Insert a report. Idempotent for Matrix-synced rows via the partial unique
/// index on (source, synapse_report_id). Returns the new row id, or None when
/// a conflict skipped the insert.
pub async fn insert_report(pool: &PgPool, r: &NewReport<'_>) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO mm_moderation_reports
           (source, target_type, target_id, room_id, reported_user_id, reporter_id, reason, details, synapse_report_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT (source, synapse_report_id) WHERE synapse_report_id IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(r.source).bind(r.target_type).bind(r.target_id).bind(r.room_id)
    .bind(r.reported_user_id).bind(r.reporter_id).bind(r.reason).bind(r.details)
    .bind(r.synapse_report_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|t| t.0))
}

pub async fn list_reports(
    pool: &PgPool, status: Option<&str>, limit: i64, offset: i64,
) -> sqlx::Result<Vec<ModerationReport>> {
    match status {
        Some(s) => sqlx::query_as::<_, ModerationReport>(
            "SELECT * FROM mm_moderation_reports WHERE status = $1
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        ).bind(s).bind(limit).bind(offset).fetch_all(pool).await,
        None => sqlx::query_as::<_, ModerationReport>(
            "SELECT * FROM mm_moderation_reports ORDER BY created_at DESC LIMIT $1 OFFSET $2",
        ).bind(limit).bind(offset).fetch_all(pool).await,
    }
}

pub async fn get_report(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ModerationReport>> {
    sqlx::query_as::<_, ModerationReport>("SELECT * FROM mm_moderation_reports WHERE id = $1")
        .bind(id).fetch_optional(pool).await
}

/// Largest ingested synapse_report_id (sync cursor); 0 when none yet.
pub async fn max_synapse_report_id(pool: &PgPool) -> sqlx::Result<i64> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT MAX(synapse_report_id) FROM mm_moderation_reports WHERE source = 'matrix'",
    ).fetch_optional(pool).await?;
    Ok(row.and_then(|t| t.0).unwrap_or(0))
}

pub async fn set_report_status(
    pool: &PgPool, id: Uuid, status: &str, resolved_by: &str,
) -> sqlx::Result<bool> {
    let res = sqlx::query(
        "UPDATE mm_moderation_reports
         SET status = $1, resolved_by = $2, resolved_at = now(), updated_at = now()
         WHERE id = $3",
    ).bind(status).bind(resolved_by).bind(id).execute(pool).await?;
    Ok(res.rows_affected() > 0)
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ModerationAction {
    pub id: Uuid,
    pub report_id: Option<Uuid>,
    pub action_type: String,
    pub target_type: String,
    pub target_id: String,
    pub operator_id: String,
    pub reason: String,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub struct NewAction<'a> {
    pub report_id: Option<Uuid>,
    pub action_type: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub operator_id: &'a str,
    pub reason: &'a str,
    pub metadata: serde_json::Value,
}

/// Append an immutable audit row. There is intentionally no update/delete fn.
pub async fn append_action(pool: &PgPool, a: &NewAction<'_>) -> sqlx::Result<Uuid> {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO mm_moderation_actions
           (report_id, action_type, target_type, target_id, operator_id, reason, metadata)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id",
    )
    .bind(a.report_id).bind(a.action_type).bind(a.target_type).bind(a.target_id)
    .bind(a.operator_id).bind(a.reason).bind(&a.metadata)
    .fetch_one(pool).await?;
    Ok(row.0)
}

pub async fn list_actions_for_target(
    pool: &PgPool, target_type: &str, target_id: &str, limit: i64,
) -> sqlx::Result<Vec<ModerationAction>> {
    sqlx::query_as::<_, ModerationAction>(
        "SELECT * FROM mm_moderation_actions
         WHERE target_type = $1 AND target_id = $2
         ORDER BY created_at DESC LIMIT $3",
    ).bind(target_type).bind(target_id).bind(limit).fetch_all(pool).await
}

pub async fn list_actions_for_report(pool: &PgPool, report_id: Uuid) -> sqlx::Result<Vec<ModerationAction>> {
    sqlx::query_as::<_, ModerationAction>(
        "SELECT * FROM mm_moderation_actions WHERE report_id = $1 ORDER BY created_at DESC",
    ).bind(report_id).fetch_all(pool).await
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct UserModeration {
    pub user_id: String,
    pub suspended: bool,
    pub suspended_at: Option<DateTime<Utc>>,
    pub suspended_by: Option<String>,
    pub reason: Option<String>,
}

pub async fn set_user_suspended(
    pool: &PgPool, user_id: &str, suspended: bool, by: &str, reason: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_user_moderation (user_id, suspended, suspended_at, suspended_by, reason)
         VALUES ($1, $2, CASE WHEN $2 THEN now() ELSE NULL END, $3, $4)
         ON CONFLICT (user_id) DO UPDATE SET
           suspended = EXCLUDED.suspended,
           suspended_at = CASE WHEN EXCLUDED.suspended THEN now() ELSE NULL END,
           suspended_by = EXCLUDED.suspended_by,
           reason = EXCLUDED.reason",
    ).bind(user_id).bind(suspended).bind(by).bind(reason).execute(pool).await?;
    Ok(())
}

pub async fn is_user_suspended(pool: &PgPool, user_id: &str) -> sqlx::Result<bool> {
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT suspended FROM mm_user_moderation WHERE user_id = $1",
    ).bind(user_id).fetch_optional(pool).await?;
    Ok(row.map(|t| t.0).unwrap_or(false))
}

pub async fn get_user_moderation(pool: &PgPool, user_id: &str) -> sqlx::Result<Option<UserModeration>> {
    sqlx::query_as::<_, UserModeration>("SELECT * FROM mm_user_moderation WHERE user_id = $1")
        .bind(user_id).fetch_optional(pool).await
}
