//! Creator analytics — per-channel earnings, viewers, donations.
//!
//! Endpoints under `/_mm/client/v1/creator/me/analytics/*`. All actions are
//! scoped to the authenticated Matrix user via [`AuthUser`]: every query
//! filters on `mm_streams.host_user_id = auth.user_id`, so a request for a
//! `room_id` the user has never hosted in returns empty data — no separate
//! ACL check needed.
//!
//! See `WorkingDirectory/analytics-plan.md` (Track B) for the design.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use mm_core::error::{ErrorCode, MMError};

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/creator/me/analytics/rooms", get(list_my_rooms))
        .route(
            "/creator/me/analytics/rooms/{matrix_room_id}/summary",
            get(room_summary),
        )
        .route(
            "/creator/me/analytics/rooms/{matrix_room_id}/timeseries",
            get(room_timeseries),
        )
        .route(
            "/creator/me/analytics/rooms/{matrix_room_id}/top-donors",
            get(room_top_donors),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Shared classification helper
// ---------------------------------------------------------------------------

/// Treat a donation row as a Lightning donation when the legacy
/// `stripe_session_id` column is null. The SQL queries below inline
/// the `IS NULL` check; this helper exists so the convention is
/// asserted by [`tests::lightning_classifier`] in one place. If we
/// ever add a real `payment_provider` column, both this helper and
/// the SQL inline checks should converge to it.
#[allow(dead_code)]
fn is_lightning(stripe_session_id: Option<&str>) -> bool {
    stripe_session_id.is_none()
}

// ---------------------------------------------------------------------------
// GET /creator/me/analytics/rooms — rooms the user has hosted in
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MyRoom {
    matrix_room_id: String,
    stream_count_30d: i64,
    donations_cents_30d: i64,
    last_stream_at: Option<DateTime<Utc>>,
}

async fn list_my_rooms(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<Vec<MyRoom>>, ApiError> {
    let pool = pool(&state)?;
    let user_id = auth.user_id.0.as_str();
    let since = Utc::now() - Duration::days(30);

    // Rooms the user has streamed in within the last 30d, with stream
    // count and donation totals from streams they hosted.
    let rows = sqlx::query(
        r#"
        SELECT
            r.matrix_room_id                        AS matrix_room_id,
            COUNT(DISTINCT s.id)                    AS stream_count,
            COALESCE(SUM(CASE WHEN d.status = 'succeeded'
                              THEN d.amount_cents ELSE 0 END), 0) AS donations_cents,
            MAX(s.started_at)                       AS last_stream_at
        FROM mm_streams       s
        JOIN mm_rooms         r ON r.id = s.room_id
        LEFT JOIN mm_donations d ON d.stream_id = s.id
                                  AND d.recipient_user_id = $1
        WHERE s.host_user_id = $1
          AND s.started_at >= $2
        GROUP BY r.matrix_room_id
        ORDER BY last_stream_at DESC NULLS LAST
        "#,
    )
    .bind(user_id)
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let result = rows
        .into_iter()
        .map(|r| MyRoom {
            matrix_room_id: r.try_get::<String, _>("matrix_room_id").unwrap_or_default(),
            stream_count_30d: r.try_get("stream_count").unwrap_or(0),
            donations_cents_30d: r.try_get("donations_cents").unwrap_or(0),
            last_stream_at: r.try_get("last_stream_at").ok(),
        })
        .collect();

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// GET /creator/me/analytics/rooms/{room}/summary — 30d KPI strip
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct RoomSummary {
    matrix_room_id: String,
    /// Total succeeded donations (lightning + stripe) in cents, 30d.
    donations_cents_30d: i64,
    /// Lightning donations in 'succeeded' state (LN preimage proof confirmed).
    lightning_paid_cents_30d: i64,
    /// Lightning donations still in 'pending' (invoice issued, no preimage seen).
    lightning_invoice_only_cents_30d: i64,
    /// Stripe donations in 'succeeded' state.
    stripe_cents_30d: i64,
    /// Number of broadcasts started in the window.
    stream_count_30d: i64,
    /// Sum of (ended_at − started_at) for ended streams, in minutes.
    /// Streams still active are excluded; user can refresh to capture them.
    stream_minutes_30d: i64,
    /// Peak `participant_count` observed across this room's streams in window.
    peak_viewers_30d: i64,
    /// Distinct donor count.
    unique_donors_30d: i64,
}

async fn room_summary(
    auth: AuthUser,
    Path(matrix_room_id): Path<String>,
    State(state): State<SharedState>,
) -> Result<Json<RoomSummary>, ApiError> {
    let pool = pool(&state)?;
    let user_id = auth.user_id.0.as_str();
    let since = Utc::now() - Duration::days(30);

    // Single round-trip: aggregate streams + donations for this user × room.
    let row = sqlx::query(
        r#"
        WITH my_streams AS (
            SELECT s.id, s.started_at, s.ended_at, s.participant_count
            FROM mm_streams s
            JOIN mm_rooms   r ON r.id = s.room_id
            WHERE r.matrix_room_id = $1
              AND s.host_user_id   = $2
              AND s.started_at    >= $3
        )
        SELECT
            (SELECT COUNT(*) FROM my_streams)                       AS stream_count,
            (SELECT COALESCE(SUM(
                EXTRACT(EPOCH FROM (COALESCE(ended_at, now()) - started_at)) / 60.0
            ), 0) FROM my_streams)::BIGINT                           AS stream_minutes,
            (SELECT COALESCE(MAX(participant_count), 0) FROM my_streams)::BIGINT
                                                                      AS peak_viewers,
            (SELECT COALESCE(SUM(d.amount_cents), 0) FROM mm_donations d
                JOIN my_streams ms ON ms.id = d.stream_id
                WHERE d.recipient_user_id = $2 AND d.status = 'succeeded')
                                                                      AS donations_cents,
            (SELECT COALESCE(SUM(d.amount_cents), 0) FROM mm_donations d
                JOIN my_streams ms ON ms.id = d.stream_id
                WHERE d.recipient_user_id = $2 AND d.status = 'succeeded'
                  AND d.stripe_session_id IS NULL)
                                                                      AS lightning_paid_cents,
            (SELECT COALESCE(SUM(d.amount_cents), 0) FROM mm_donations d
                JOIN my_streams ms ON ms.id = d.stream_id
                WHERE d.recipient_user_id = $2 AND d.status = 'pending'
                  AND d.stripe_session_id IS NULL)
                                                                      AS lightning_invoice_only_cents,
            (SELECT COALESCE(SUM(d.amount_cents), 0) FROM mm_donations d
                JOIN my_streams ms ON ms.id = d.stream_id
                WHERE d.recipient_user_id = $2 AND d.status = 'succeeded'
                  AND d.stripe_session_id IS NOT NULL)
                                                                      AS stripe_cents,
            (SELECT COUNT(DISTINCT d.donor_user_id) FROM mm_donations d
                JOIN my_streams ms ON ms.id = d.stream_id
                WHERE d.recipient_user_id = $2 AND d.status = 'succeeded')
                                                                      AS unique_donors
        "#,
    )
    .bind(&matrix_room_id)
    .bind(user_id)
    .bind(since)
    .fetch_one(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(RoomSummary {
        matrix_room_id,
        donations_cents_30d: row.try_get("donations_cents").unwrap_or(0),
        lightning_paid_cents_30d: row.try_get("lightning_paid_cents").unwrap_or(0),
        lightning_invoice_only_cents_30d: row
            .try_get("lightning_invoice_only_cents")
            .unwrap_or(0),
        stripe_cents_30d: row.try_get("stripe_cents").unwrap_or(0),
        stream_count_30d: row.try_get("stream_count").unwrap_or(0),
        stream_minutes_30d: row.try_get("stream_minutes").unwrap_or(0),
        peak_viewers_30d: row.try_get("peak_viewers").unwrap_or(0),
        unique_donors_30d: row.try_get("unique_donors").unwrap_or(0),
    }))
}

// ---------------------------------------------------------------------------
// GET /creator/me/analytics/rooms/{room}/timeseries
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TimeseriesQuery {
    /// One of: donations | streams | viewers
    metric: String,
    /// One of: 7d | 30d | 90d (default 30d)
    #[serde(default = "default_range")]
    range: String,
}

fn default_range() -> String {
    "30d".to_string()
}

#[derive(Debug, Serialize)]
struct TimeseriesPoint {
    /// Bucket start (UTC, midnight).
    ts: DateTime<Utc>,
    value: f64,
}

#[derive(Debug, Serialize)]
struct TimeseriesResponse {
    metric: String,
    range: String,
    unit: &'static str,
    buckets: Vec<TimeseriesPoint>,
}

async fn room_timeseries(
    auth: AuthUser,
    Path(matrix_room_id): Path<String>,
    Query(q): Query<TimeseriesQuery>,
    State(state): State<SharedState>,
) -> Result<Json<TimeseriesResponse>, ApiError> {
    let pool = pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    let days = match q.range.as_str() {
        "7d" => 7,
        "90d" => 90,
        _ => 30, // includes "30d" and bad input
    };
    let since = Utc::now() - Duration::days(days);

    // Per-metric SQL. All three filter by host_user_id so a stranger's
    // request returns an empty series for any room they don't host.
    let (sql, unit): (&str, &'static str) = match q.metric.as_str() {
        "donations" => (
            r#"
            SELECT date_trunc('day', d.created_at) AS bucket,
                   COALESCE(SUM(d.amount_cents), 0)::DOUBLE PRECISION AS value
            FROM mm_donations d
            JOIN mm_streams   s ON s.id = d.stream_id
            JOIN mm_rooms     r ON r.id = s.room_id
            WHERE r.matrix_room_id  = $1
              AND s.host_user_id    = $2
              AND d.recipient_user_id = $2
              AND d.status          = 'succeeded'
              AND d.created_at      >= $3
            GROUP BY bucket
            ORDER BY bucket
            "#,
            "cents",
        ),
        "streams" => (
            r#"
            SELECT date_trunc('day', s.started_at) AS bucket,
                   COUNT(*)::DOUBLE PRECISION       AS value
            FROM mm_streams s
            JOIN mm_rooms   r ON r.id = s.room_id
            WHERE r.matrix_room_id = $1
              AND s.host_user_id   = $2
              AND s.started_at    >= $3
            GROUP BY bucket
            ORDER BY bucket
            "#,
            "count",
        ),
        "viewers" => (
            r#"
            SELECT date_trunc('day', s.started_at) AS bucket,
                   MAX(s.participant_count)::DOUBLE PRECISION AS value
            FROM mm_streams s
            JOIN mm_rooms   r ON r.id = s.room_id
            WHERE r.matrix_room_id = $1
              AND s.host_user_id   = $2
              AND s.started_at    >= $3
            GROUP BY bucket
            ORDER BY bucket
            "#,
            "peak",
        ),
        _ => {
            return Err(MMError::api(
                ErrorCode::InvalidRequest,
                format!("unknown metric '{}'; want donations|streams|viewers", q.metric),
            )
            .into());
        }
    };

    let rows = sqlx::query(sql)
        .bind(&matrix_room_id)
        .bind(user_id)
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    let buckets = rows
        .into_iter()
        .map(|r| TimeseriesPoint {
            ts: r.try_get("bucket").unwrap_or_else(|_| Utc::now()),
            value: r.try_get("value").unwrap_or(0.0),
        })
        .collect();

    Ok(Json(TimeseriesResponse {
        metric: q.metric,
        range: q.range,
        unit,
        buckets,
    }))
}

// ---------------------------------------------------------------------------
// GET /creator/me/analytics/rooms/{room}/top-donors
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TopDonorsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    10
}

#[derive(Debug, Serialize)]
struct TopDonor {
    donor_user_id: String,
    /// Total cents across all confirmed donations (LN + Stripe).
    total_cents: i64,
    /// Number of distinct donations.
    donation_count: i64,
    /// Most-recent donation timestamp.
    last_at: Option<DateTime<Utc>>,
    /// True if every donation in the rollup was Lightning. Lets the UI
    /// render a ⚡ badge instead of a $ when appropriate.
    lightning_only: bool,
}

async fn room_top_donors(
    auth: AuthUser,
    Path(matrix_room_id): Path<String>,
    Query(q): Query<TopDonorsQuery>,
    State(state): State<SharedState>,
) -> Result<Json<Vec<TopDonor>>, ApiError> {
    let pool = pool(&state)?;
    let user_id = auth.user_id.0.as_str();
    let since = Utc::now() - Duration::days(30);
    let limit = q.limit.clamp(1, 100);

    let rows = sqlx::query(
        r#"
        SELECT
            d.donor_user_id                                        AS donor_user_id,
            SUM(d.amount_cents)                                    AS total_cents,
            COUNT(*)                                               AS donation_count,
            MAX(d.created_at)                                      AS last_at,
            BOOL_AND(d.stripe_session_id IS NULL)                  AS lightning_only
        FROM mm_donations d
        JOIN mm_streams   s ON s.id = d.stream_id
        JOIN mm_rooms     r ON r.id = s.room_id
        WHERE r.matrix_room_id   = $1
          AND s.host_user_id     = $2
          AND d.recipient_user_id = $2
          AND d.status           = 'succeeded'
          AND d.created_at       >= $3
        GROUP BY d.donor_user_id
        ORDER BY total_cents DESC
        LIMIT $4
        "#,
    )
    .bind(&matrix_room_id)
    .bind(user_id)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let result = rows
        .into_iter()
        .map(|r| TopDonor {
            donor_user_id: r.try_get("donor_user_id").unwrap_or_default(),
            total_cents: r.try_get("total_cents").unwrap_or(0),
            donation_count: r.try_get("donation_count").unwrap_or(0),
            last_at: r.try_get("last_at").ok(),
            lightning_only: r.try_get("lightning_only").unwrap_or(false),
        })
        .collect();

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------------

fn pool(state: &SharedState) -> Result<&sqlx::PgPool, ApiError> {
    state
        .pg_pool
        .as_ref()
        .ok_or_else(|| {
            MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled").into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightning_classifier() {
        assert!(is_lightning(None));
        assert!(!is_lightning(Some("cs_test_xyz")));
        assert!(!is_lightning(Some("")));
    }

    #[test]
    fn default_range_value() {
        assert_eq!(default_range(), "30d");
    }

    #[test]
    fn default_limit_value() {
        assert_eq!(default_limit(), 10);
    }
}
