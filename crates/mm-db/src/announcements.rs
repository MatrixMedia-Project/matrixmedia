//! Database access for `mm_announcements` (server-announcement banners).
//!
//! Operators publish short, severity-tagged messages via the admin API;
//! every active client polls `GET /mm/v1/announcements/active` every 60s
//! and surfaces the highest-severity active banner.
//!
//! The active-selection ordering is `critical > warning > info`, then
//! `starts_at DESC`. Rows whose `expires_at <= now()` are ignored.
//!
//! Backed by `signup_pool` (the always-present `PgPool` on `AppState`) so
//! the feature works on deployments that have monetization disabled.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

/// A row from `mm_announcements` returned to API consumers.
///
/// The `serde::Serialize` impl produces the JSON shape that the public
/// `GET /mm/v1/announcements/active` handler and the admin list endpoint
/// return on the wire. `cta_label` / `cta_url` are skipped when `None` to
/// keep the payload compact for the common "plain text banner" case.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AnnouncementRow {
    pub id: i64,
    pub severity: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cta_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cta_url: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub dismissible: bool,
    /// Operator (Matrix user id) who created the row, when available.
    /// `None` for legacy rows and for token-auth callers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// Input for `create()`. Borrowed `&str`s to keep callers allocation-free.
#[derive(Debug)]
pub struct CreateAnnouncement<'a> {
    pub severity: &'a str,
    pub body: &'a str,
    pub cta_label: Option<&'a str>,
    pub cta_url: Option<&'a str>,
    /// If `None`, the row defaults to `starts_at = now()` (so the banner is
    /// immediately active). Use `Some(...)` to schedule for the future.
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub dismissible: bool,
    pub created_by: Option<&'a str>,
}

/// Return the single highest-severity currently-active row, if any.
///
/// "Active" means `starts_at <= now() AND expires_at > now()`. When several
/// rows are active simultaneously, severity wins (critical > warning > info)
/// and ties are broken by `starts_at DESC` so the most recently-scheduled
/// banner takes precedence.
pub async fn get_active(pool: &PgPool) -> sqlx::Result<Option<AnnouncementRow>> {
    let row = sqlx::query_as::<_, AnnouncementRow>(
        "SELECT id, severity, body, cta_label, cta_url, expires_at, dismissible, created_by \
         FROM mm_announcements \
         WHERE starts_at <= now() AND expires_at > now() \
         ORDER BY CASE severity \
                    WHEN 'critical' THEN 0 \
                    WHEN 'warning'  THEN 1 \
                    ELSE 2 \
                  END, \
                  starts_at DESC \
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Insert a new announcement and return its `id`.
///
/// Validation (severity, body length, expires_at in future) is the caller's
/// responsibility — this function just writes the row. The PG `CHECK`
/// constraints catch defence-in-depth violations (returns an SQL error).
pub async fn create(pool: &PgPool, req: &CreateAnnouncement<'_>) -> sqlx::Result<i64> {
    let id: (i64,) = sqlx::query_as(
        "INSERT INTO mm_announcements \
           (severity, body, cta_label, cta_url, starts_at, expires_at, dismissible, created_by) \
         VALUES ($1, $2, $3, $4, COALESCE($5, now()), $6, $7, $8) \
         RETURNING id",
    )
    .bind(req.severity)
    .bind(req.body)
    .bind(req.cta_label)
    .bind(req.cta_url)
    .bind(req.starts_at)
    .bind(req.expires_at)
    .bind(req.dismissible)
    .bind(req.created_by)
    .fetch_one(pool)
    .await?;
    Ok(id.0)
}

/// Force-expire an announcement (set `expires_at = now()`).
///
/// Returns `true` when a row was updated, `false` if no row matched `id`.
/// The admin DELETE handler relies on this to distinguish 200 vs 404.
pub async fn expire_now(pool: &PgPool, id: i64) -> sqlx::Result<bool> {
    let res = sqlx::query("UPDATE mm_announcements SET expires_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// List the most recent announcements (regardless of active state) for the
/// admin list view. `limit` caps the result set.
pub async fn list_all(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<AnnouncementRow>> {
    let rows = sqlx::query_as::<_, AnnouncementRow>(
        "SELECT id, severity, body, cta_label, cta_url, expires_at, dismissible, created_by \
         FROM mm_announcements \
         ORDER BY id DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
