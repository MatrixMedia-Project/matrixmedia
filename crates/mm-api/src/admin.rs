use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mm_core::error::{ErrorCode, MMError};
use mm_core::types::{StreamId, StreamStatus};
use mm_db::models::RecordingStatus;
use mm_matrix::events;

use crate::client::{RecordingResponse, delete_recording_storage};
use crate::error::ApiError;
use crate::middleware::AdminAuth;
use crate::state::SharedState;

/// Build admin API routes.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/streams", get(list_streams))
        .route("/streams/{id}", delete(force_stop_stream))
        .route("/config", get(get_config))
        .route("/config", put(set_config))
        .route("/recordings", get(admin_list_recordings))
        .route("/recordings/{id}", delete(admin_delete_recording))
        .route("/recordings/cleanup", post(admin_cleanup_recordings))
        // Payment admin
        .route("/donations", get(admin_list_donations))
        .route("/donations/{id}/status", put(admin_update_donation_status))
        .route("/creators/{user_id}/onboarding", put(admin_set_onboarding))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Health check response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    checks: HealthChecks,
}

#[derive(Debug, Serialize)]
struct HealthChecks {
    database: ComponentHealth,
    homeserver: ComponentHealth,
    sfu: ComponentHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    redis: Option<ComponentHealth>,
}

#[derive(Debug, Serialize)]
struct ComponentHealth {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Stats response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StatsResponse {
    active_streams: usize,
    active_participants: usize,
    uptime_seconds: u64,
}

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SetConfigRequest {
    key: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct ConfigEntryResponse {
    key: String,
    value: String,
    updated_at: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /health -- Component-level health check.
///
/// Checks DB, SFU, and homeserver connectivity. Returns per-component status
/// with latency measurements.
async fn health(_admin: AdminAuth, State(state): State<SharedState>) -> Json<HealthResponse> {
    // Check database.
    let db_health = {
        let start = std::time::Instant::now();
        match state.db.health_check().await {
            Ok(()) => ComponentHealth {
                status: "ok".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            },
            Err(e) => ComponentHealth {
                status: "error".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{e}")),
            },
        }
    };

    // Check homeserver.
    let hs_health = {
        let start = std::time::Instant::now();
        match state.hs_client.whoami().await {
            Ok(_) => ComponentHealth {
                status: "ok".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            },
            Err(e) => ComponentHealth {
                status: "error".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{e}")),
            },
        }
    };

    // Check SFU.
    let sfu_health = {
        let start = std::time::Instant::now();
        match state.sfu.health_check().await {
            Ok(()) => ComponentHealth {
                status: "ok".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            },
            Err(e) => ComponentHealth {
                status: "error".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{e}")),
            },
        }
    };

    // Check Redis (if configured).
    let redis_health = if let Some(ref redis) = state.redis {
        let start = std::time::Instant::now();
        match redis.ping().await {
            Ok(()) => Some(ComponentHealth {
                status: "ok".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            }),
            Err(e) => Some(ComponentHealth {
                status: "error".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: Some(format!("{e}")),
            }),
        }
    } else {
        None
    };

    let redis_ok = redis_health
        .as_ref()
        .map(|h| h.status == "ok")
        .unwrap_or(true); // Not configured = not degraded.

    let overall = if db_health.status == "ok"
        && hs_health.status == "ok"
        && sfu_health.status == "ok"
        && redis_ok
    {
        "ok"
    } else {
        "degraded"
    };

    Json(HealthResponse {
        status: overall.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        checks: HealthChecks {
            database: db_health,
            homeserver: hs_health,
            sfu: sfu_health,
            redis: redis_health,
        },
    })
}

/// GET /stats -- Server statistics.
///
/// Returns the number of active streams and total active participants,
/// derived from the Prometheus metrics (real-time gauges).
async fn stats(
    _admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<StatsResponse>, ApiError> {
    // Query active streams from DB to get accurate counts after restart
    let active_streams = state.db.list_all_active_streams(1000).await?;
    let active_participants: i64 = active_streams
        .iter()
        .map(|s| s.participant_count as i64)
        .sum();
    Ok(Json(StatsResponse {
        active_streams: active_streams.len(),
        active_participants: active_participants as usize,
        uptime_seconds: state.started_at.elapsed().as_secs(),
    }))
}

/// GET /streams -- All active streams (admin view).
async fn list_streams(
    _admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let streams = state.db.list_all_active_streams(100).await?;
    let items: Vec<_> = streams
        .into_iter()
        .map(|s| {
            json!({
                "stream_id": s.id,
                "room_id": s.room_id.to_string(),
                "host": s.host_user_id,
                "media_type": s.media_type,
                "title": s.title,
                "status": s.status,
                "participant_count": s.participant_count,
                "started_at": s.started_at.to_rfc3339(),
                "ended_at": s.ended_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();
    Ok(Json(json!({ "streams": items })))
}

/// DELETE /streams/:id -- Force-stop a stream (admin privilege, no host check).
async fn force_stop_stream(
    _admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    // Delete SFU room (best-effort).
    if let Some(ref sfu_room_id) = stream.sfu_room_id {
        let _ = state.sfu.delete_room(sfu_room_id).await;
    }

    // Update stream status.
    state
        .db
        .update_stream_status(&stream_id, StreamStatus::Ended)
        .await?;

    // Clear stream state event in Matrix.
    if let Some(room) = state.db.get_room(stream.room_id).await? {
        let _ = events::clear_stream_active(&state.hs_client, &room.matrix_room_id).await;

        let duration_secs = chrono::Utc::now()
            .signed_duration_since(stream.started_at)
            .num_seconds()
            .max(0) as u64;

        let _ = events::notify_stream_ended(
            &state.hs_client,
            &room.matrix_room_id,
            &stream.host_user_id,
            duration_secs,
            stream.participant_count as u32,
        )
        .await;
    }

    Ok(Json(json!({ "ok": true })))
}

/// GET /config -- Read server configuration (key-value store).
async fn get_config(
    _admin: AdminAuth,
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let key = params.get("key").cloned().unwrap_or_default();
    if key.is_empty() {
        // Return a summary of known config keys.
        return Ok(Json(json!({
            "hint": "provide ?key=<key> to query a specific config value"
        })));
    }

    match state.db.get_config(&key).await? {
        Some(entry) => Ok(Json(json!(ConfigEntryResponse {
            key: entry.key,
            value: entry.value,
            updated_at: entry.updated_at.to_rfc3339(),
        }))),
        None => {
            Err(MMError::api(ErrorCode::NotFound, format!("config key not found: {key}")).into())
        }
    }
}

/// PUT /config -- Update server configuration.
async fn set_config(
    _admin: AdminAuth,
    State(state): State<SharedState>,
    Json(body): Json<SetConfigRequest>,
) -> Result<Json<Value>, ApiError> {
    state.db.set_config(&body.key, &body.value).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Recording admin endpoints
// ---------------------------------------------------------------------------

/// Query parameters for the admin recording list endpoint.
#[derive(Debug, Deserialize)]
struct AdminRecordingListParams {
    /// Optional status filter (`ready`, `processing`, `recording`, `failed`, `deleted`).
    status: Option<String>,
    /// Maximum number of items to return (default 100, max 500).
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct AdminRecordingListResponse {
    recordings: Vec<RecordingResponse>,
}

#[derive(Debug, Serialize)]
struct CleanupResponse {
    deleted: usize,
    retention_days: u32,
}

const ADMIN_DEFAULT_LIMIT: i64 = 100;
const ADMIN_MAX_LIMIT: i64 = 500;

/// GET /_mm/admin/v1/recordings -- List all recordings (admin).
async fn admin_list_recordings(
    _admin: AdminAuth,
    State(state): State<SharedState>,
    Query(params): Query<AdminRecordingListParams>,
) -> Result<Json<AdminRecordingListResponse>, ApiError> {
    let limit = params
        .limit
        .unwrap_or(ADMIN_DEFAULT_LIMIT)
        .clamp(1, ADMIN_MAX_LIMIT) as u32;

    let status_filter = params.status.as_deref().filter(|s| !s.is_empty());

    let rows = state.db.list_all_recordings(limit, status_filter).await?;
    let recordings = rows.into_iter().map(RecordingResponse::from).collect();

    Ok(Json(AdminRecordingListResponse { recordings }))
}

/// DELETE /_mm/admin/v1/recordings/:id -- Force-delete a recording (admin).
async fn admin_delete_recording(
    _admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let recording = state
        .db
        .get_recording(&id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "recording not found"))?;

    if recording.status == RecordingStatus::Deleted.as_str() {
        return Err(MMError::api(ErrorCode::NotFound, "recording not found").into());
    }

    delete_recording_storage(&state, &recording).await;

    state
        .db
        .update_recording_status(&recording.id, RecordingStatus::Deleted)
        .await?;

    Ok(Json(json!({ "ok": true })))
}

/// POST /_mm/admin/v1/recordings/cleanup -- Trigger retention cleanup.
///
/// Deletes all recordings older than `recording.retention_days`. When
/// `retention_days = 0`, retention is disabled and the endpoint returns
/// `deleted: 0` without touching any rows.
async fn admin_cleanup_recordings(
    _admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<CleanupResponse>, ApiError> {
    let retention_days = state.config.recording.retention_days;
    if retention_days == 0 {
        return Ok(Json(CleanupResponse {
            deleted: 0,
            retention_days: 0,
        }));
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
    let cutoff_str = cutoff.to_rfc3339();

    let rows = state.db.recordings_older_than(&cutoff_str).await?;
    let mut deleted = 0usize;

    for recording in rows {
        delete_recording_storage(&state, &recording).await;
        match state
            .db
            .update_recording_status(&recording.id, RecordingStatus::Deleted)
            .await
        {
            Ok(()) => deleted += 1,
            Err(e) => {
                tracing::warn!(
                    recording_id = %recording.id,
                    "Failed to mark recording as deleted: {e}"
                );
            }
        }
    }

    Ok(Json(CleanupResponse {
        deleted,
        retention_days,
    }))
}

// ---------------------------------------------------------------------------
// Payment Admin
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DonationListQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
}

async fn admin_list_donations(
    _auth: AdminAuth,
    State(state): State<SharedState>,
    Query(q): Query<DonationListQuery>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let limit = q.limit.unwrap_or(50).min(200);
    let rows = if let Some(ref status) = q.status {
        sqlx::query_as::<_, mm_db::models::Donation>(
            "SELECT * FROM mm_donations WHERE status = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(status)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
    } else {
        sqlx::query_as::<_, mm_db::models::Donation>(
            "SELECT * FROM mm_donations ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
    };

    let donations: Vec<Value> = rows
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "stream_id": d.stream_id,
                "donor_user_id": d.donor_user_id,
                "recipient_user_id": d.recipient_user_id,
                "amount_cents": d.amount_cents,
                "currency": d.currency,
                "message": d.message,
                "tier": d.tier,
                "status": d.status,
                "created_at": d.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!({ "donations": donations, "count": donations.len() })))
}

#[derive(Debug, Deserialize)]
pub struct UpdateStatusBody {
    pub status: String,
}

async fn admin_update_donation_status(
    _auth: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateStatusBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let valid = ["pending", "succeeded", "failed", "refunded"];
    if !valid.contains(&body.status.as_str()) {
        return Err(MMError::api(ErrorCode::InvalidAmount, &format!("Invalid status. Must be one of: {}", valid.join(", "))).into());
    }

    sqlx::query("UPDATE mm_donations SET status = $1 WHERE id = $2::uuid")
        .bind(&body.status)
        .bind(&id)
        .execute(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(json!({ "ok": true, "donation_id": id, "new_status": body.status })))
}

#[derive(Debug, Deserialize)]
pub struct OnboardingBody {
    pub onboarding_complete: bool,
}

async fn admin_set_onboarding(
    _auth: AdminAuth,
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
    Json(body): Json<OnboardingBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    sqlx::query("UPDATE mm_creator_profiles SET onboarding_complete = $1, updated_at = now() WHERE user_id = $2")
        .bind(body.onboarding_complete)
        .bind(&user_id)
        .execute(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(json!({ "ok": true, "user_id": user_id, "onboarding_complete": body.onboarding_complete })))
}
