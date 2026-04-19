use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mm_core::auth::issue_admin_session_token;
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::{StreamId, StreamStatus};
use mm_db::models::RecordingStatus;
use mm_matrix::events;

use crate::client::{RecordingResponse, delete_recording_storage};
use crate::error::ApiError;
use crate::middleware::{AdminAuth, AdminRole};
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
        .route("/subscriptions", get(admin_list_subscriptions))
        .route("/content-gates", get(admin_list_content_gates))
        .route("/content-gates/{id}", delete(admin_remove_content_gate))
        .route("/creators", get(admin_list_creators))
        .route("/creators/{user_id}/onboarding", put(admin_set_onboarding))
        // Phase 9: Operator Console platform endpoints
        .route("/platform/metrics-summary", get(platform_metrics_summary))
        .route("/platform/revenue", get(platform_revenue))
        .route("/platform/federation", get(platform_federation))
        .route("/platform/config-full", get(platform_config_full))
        .route("/platform/deployment", get(platform_deployment))
        // Synapse admin proxy (server-side — no Synapse creds in browser)
        .route("/synapse/users", get(synapse_list_users))
        .route("/synapse/users/{user_id}", put(synapse_upsert_user))
        .route("/synapse/deactivate/{user_id}", post(synapse_deactivate_user))
        // Phase 9: Advertising admin
        .route("/ads", get(admin_list_ads))
        .route("/ads", post(admin_upload_ad))
        .route("/ads/{id}", put(admin_update_ad))
        .route("/ads/{id}", delete(admin_delete_ad))
        .route("/ads/{id}/upload", post(admin_upload_ad_file)
            .layer(DefaultBodyLimit::max(200 * 1024 * 1024))) // 200MB for video uploads
        .route("/ads/{id}/stats", get(admin_get_ad_stats))
        .route("/ads/analytics", get(admin_ad_analytics))
        // Dashboard auth endpoints (no AdminAuth required)
        .route("/auth-info", get(auth_info))
        .route("/login", post(admin_login))
        // System health (requires AdminAuth)
        .route("/system-health", get(system_health))
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
    // Health is safe for all roles (admin and demo).
    // Check database.
    let db_health = {
        let start = std::time::Instant::now();
        match state.db.health_check().await {
            Ok(()) => ComponentHealth {
                status: "ok".to_string(),
                latency_ms: Some(start.elapsed().as_millis() as u64),
                error: None,
            },
            Err(e) => {
                tracing::error!(component = "database", error = %e, "Health check failed");
                ComponentHealth {
                    status: "error".to_string(),
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some("service unavailable".to_string()),
                }
            }
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
            Err(e) => {
                tracing::error!(component = "homeserver", error = %e, "Health check failed");
                ComponentHealth {
                    status: "error".to_string(),
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some("service unavailable".to_string()),
                }
            }
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
            Err(e) => {
                tracing::error!(component = "sfu", error = %e, "Health check failed");
                ComponentHealth {
                    status: "error".to_string(),
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some("service unavailable".to_string()),
                }
            }
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
            Err(e) => {
                tracing::error!(component = "redis", error = %e, "Health check failed");
                Some(ComponentHealth {
                    status: "error".to_string(),
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    error: Some("service unavailable".to_string()),
                })
            }
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
    _admin: AdminAuth, // safe for all roles
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

/// GET /streams -- All active streams (admin view). Safe for demo role.
async fn list_streams(
    _admin: AdminAuth, // safe for all roles
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
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
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
    admin: AdminAuth,
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Ok(Json(json!({ "demo": true })));
    }
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
    admin: AdminAuth,
    State(state): State<SharedState>,
    Json(body): Json<SetConfigRequest>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
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
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
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
    admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<CleanupResponse>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
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
                "creator_user_id": d.recipient_user_id,
                "amount_cents": d.amount_cents,
                "currency": d.currency,
                "message": d.message,
                "tier": d.tier,
                "status": d.status,
                "provider": if d.stripe_session_id.is_some() { "stripe" } else { "lightning" },
                "created_at": d.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(
        json!({ "donations": donations, "count": donations.len() }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct UpdateStatusBody {
    pub status: String,
}

async fn admin_update_donation_status(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateStatusBody>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let valid = ["pending", "succeeded", "failed", "refunded"];
    if !valid.contains(&body.status.as_str()) {
        return Err(MMError::api(
            ErrorCode::InvalidAmount,
            format!("Invalid status. Must be one of: {}", valid.join(", ")),
        )
        .into());
    }

    sqlx::query("UPDATE mm_donations SET status = $1 WHERE id = $2::uuid")
        .bind(&body.status)
        .bind(&id)
        .execute(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(
        json!({ "ok": true, "donation_id": id, "new_status": body.status }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionListQuery {
    pub status: Option<String>,
    pub limit: Option<i64>,
}

async fn admin_list_subscriptions(
    _auth: AdminAuth,
    State(state): State<SharedState>,
    Query(q): Query<SubscriptionListQuery>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let status_filter = q.status.as_deref().filter(|s| *s != "all");

    let sql = "SELECT s.id, s.subscriber_user_id, s.creator_user_id, s.status, \
               s.current_period_end, s.created_at, \
               t.name AS tier_name, t.tier_level, t.price_cents, t.currency \
               FROM mm_subscriptions s \
               JOIN mm_subscription_tiers t ON s.tier_id = t.id";

    let rows = if let Some(status) = status_filter {
        let q_sql = format!("{sql} WHERE s.status = $1 ORDER BY s.created_at DESC LIMIT $2");
        sqlx::query(&q_sql)
            .bind(status)
            .bind(limit)
            .fetch_all(pool)
            .await
    } else {
        let q_sql = format!("{sql} ORDER BY s.created_at DESC LIMIT $1");
        sqlx::query(&q_sql).bind(limit).fetch_all(pool).await
    }
    .map_err(|e| MMError::Database(e.to_string()))?;

    use sqlx::Row;
    let subscriptions: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
                "subscriber_user_id": r.try_get::<String, _>("subscriber_user_id").unwrap_or_default(),
                "creator_user_id": r.try_get::<String, _>("creator_user_id").unwrap_or_default(),
                "tier_name": r.try_get::<String, _>("tier_name").unwrap_or_default(),
                "tier_level": r.try_get::<i32, _>("tier_level").unwrap_or(0),
                "status": r.try_get::<String, _>("status").unwrap_or_default(),
                "price_cents": r.try_get::<i64, _>("price_cents").unwrap_or(0),
                "currency": r.try_get::<String, _>("currency").unwrap_or_else(|_| "usd".to_owned()),
                "current_period_end": r.try_get::<chrono::DateTime<chrono::Utc>, _>("current_period_end")
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
                "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(Json(
        json!({ "subscriptions": subscriptions, "count": subscriptions.len() }),
    ))
}

async fn admin_list_content_gates(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    // Left-join the creator's tier at min_tier_level so we can return tier_name.
    let rows = sqlx::query(
        "SELECT g.id, g.content_type, g.content_id, g.creator_user_id, \
         g.min_tier_level, g.preview_seconds, g.created_at, \
         COALESCE(t.name, '') AS tier_name \
         FROM mm_content_gates g \
         LEFT JOIN mm_subscription_tiers t \
           ON t.creator_user_id = g.creator_user_id AND t.tier_level = g.min_tier_level \
         ORDER BY g.created_at DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    use sqlx::Row;
    let content_gates: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
                "content_type": r.try_get::<String, _>("content_type").unwrap_or_default(),
                "content_id": r.try_get::<String, _>("content_id").unwrap_or_default(),
                "creator_user_id": r.try_get::<String, _>("creator_user_id").unwrap_or_default(),
                "required_tier_name": r.try_get::<String, _>("tier_name").unwrap_or_default(),
                "required_tier_level": r.try_get::<i32, _>("min_tier_level").unwrap_or(0),
                "preview_seconds": r.try_get::<i32, _>("preview_seconds").unwrap_or(0),
                "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(Json(
        json!({ "content_gates": content_gates, "count": content_gates.len() }),
    ))
}

async fn admin_remove_content_gate(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let result = sqlx::query("DELETE FROM mm_content_gates WHERE id = $1::uuid")
        .bind(&id)
        .execute(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(MMError::api(ErrorCode::NotFound, "content gate not found").into());
    }
    Ok(Json(json!({ "ok": true, "id": id })))
}

#[derive(Debug, Deserialize)]
pub struct OnboardingBody {
    pub onboarding_complete: bool,
}

async fn admin_set_onboarding(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
    Json(body): Json<OnboardingBody>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
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

    Ok(Json(
        json!({ "ok": true, "user_id": user_id, "onboarding_complete": body.onboarding_complete }),
    ))
}

/// GET /admin/v1/creators -- List all creator profiles with onboarding status.
async fn admin_list_creators(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let rows = sqlx::query_as::<_, mm_db::models::CreatorProfile>(
        "SELECT id, user_id, display_name, stripe_account_id, onboarding_complete,
                platform_fee_pct::float8, created_at, updated_at
         FROM mm_creator_profiles
         ORDER BY created_at DESC
         LIMIT 200",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let creators: Vec<Value> = rows
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "user_id": c.user_id,
                "display_name": c.display_name,
                "stripe_account_id": c.stripe_account_id,
                "onboarding_complete": c.onboarding_complete,
                "platform_fee_pct": c.platform_fee_pct,
                "created_at": c.created_at.to_rfc3339(),
                "updated_at": c.updated_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(
        json!({ "creators": creators, "count": creators.len() }),
    ))
}

// ===========================================================================
// Phase 9: Operator Console platform endpoints
// ===========================================================================
//
// These are read-only aggregation endpoints that the mm-operator-console
// demo calls to render the admin views. Everything here is either computed
// from existing state (config, metrics) or pulled from the PG pool.

/// GET /platform/metrics-summary — top-line counters for the console header.
async fn platform_metrics_summary(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let uptime_secs = state.started_at.elapsed().as_secs();

    // Stream + participant counts via direct SQL on the PG pool. If
    // monetization is off there's no PG pool, so report zeros.
    let (active_streams, active_participants, donations_total_cents, subscriptions_active) =
        if let Some(pool) = state.pg_pool.as_ref() {
            let streams: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM mm_streams WHERE status = 'active'",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            let participants: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM mm_participants WHERE left_at IS NULL",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            let donations: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(amount_cents), 0)::BIGINT FROM mm_donations WHERE status = 'succeeded'",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            let subs: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)::BIGINT FROM mm_subscriptions WHERE status = 'active'",
            )
            .fetch_one(pool)
            .await
            .unwrap_or(0);
            (streams, participants, donations, subs)
        } else {
            (0i64, 0i64, 0i64, 0i64)
        };

    Ok(Json(json!({
        "uptime_seconds": uptime_secs,
        "active_streams": active_streams,
        "active_participants": active_participants,
        "donations_total_cents": donations_total_cents,
        "subscriptions_active": subscriptions_active,
        "monetization_enabled": state.config.monetization.enabled,
        "subscriptions_enabled": state.config.monetization.subscriptions_enabled,
        "donations_enabled": state.config.monetization.donations_enabled,
    })))
}

/// GET /platform/revenue — revenue rollups for the last 30 days.
async fn platform_revenue(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let Some(pool) = state.pg_pool.as_ref() else {
        return Ok(Json(json!({
            "enabled": false,
            "reason": "monetization disabled",
            "by_day": [],
            "totals": { "gross_cents": 0, "fee_cents": 0, "net_cents": 0 },
        })));
    };

    let platform_fee_pct = state.config.monetization.platform_fee_pct;

    // Gross donations by day for the last 30 days.
    #[derive(sqlx::FromRow)]
    struct DayRow {
        day: chrono::DateTime<chrono::Utc>,
        gross_cents: i64,
        donation_count: i64,
    }
    let rows: Vec<DayRow> = sqlx::query_as::<_, DayRow>(
        "SELECT date_trunc('day', created_at)::timestamptz AS day,
                COALESCE(SUM(amount_cents), 0)::BIGINT AS gross_cents,
                COUNT(*)::BIGINT AS donation_count
         FROM mm_donations
         WHERE status = 'succeeded'
           AND created_at > now() - INTERVAL '30 days'
         GROUP BY day
         ORDER BY day DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let by_day: Vec<Value> = rows
        .iter()
        .map(|r| {
            let fee = ((r.gross_cents as f64) * platform_fee_pct) as i64;
            json!({
                "day": r.day.to_rfc3339(),
                "gross_cents": r.gross_cents,
                "fee_cents": fee,
                "net_cents": r.gross_cents - fee,
                "donation_count": r.donation_count,
            })
        })
        .collect();

    let total_gross: i64 = rows.iter().map(|r| r.gross_cents).sum();
    let total_fee = ((total_gross as f64) * platform_fee_pct) as i64;

    Ok(Json(json!({
        "enabled": true,
        "window_days": 30,
        "platform_fee_pct": platform_fee_pct,
        "by_day": by_day,
        "totals": {
            "gross_cents": total_gross,
            "fee_cents": total_fee,
            "net_cents": total_gross - total_fee,
        },
    })))
}

/// GET /platform/federation — federation health + remote server summary.
async fn platform_federation(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let local_server = state.config.matrix.server_name.clone();
    let allow_list = state.config.federation.allow_list.clone();
    let deny_list = state.config.federation.deny_list.clone();

    // Count federated joins from the metrics registry.
    let federated_joins = state.metrics.federated_joins_total.get();

    // Remote server breakdown (best-effort from mm_participants).
    let remote_servers: Vec<Value> = if let Some(pool) = state.pg_pool.as_ref() {
        #[derive(sqlx::FromRow)]
        struct ServerRow {
            server: String,
            viewers: i64,
        }
        let rows: Vec<ServerRow> = sqlx::query_as::<_, ServerRow>(
            "SELECT split_part(user_id, ':', 2) AS server,
                    COUNT(*)::BIGINT AS viewers
             FROM mm_participants
             WHERE left_at IS NULL
               AND split_part(user_id, ':', 2) != $1
             GROUP BY server
             ORDER BY viewers DESC
             LIMIT 50",
        )
        .bind(&local_server)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.iter()
            .map(|r| json!({ "server": r.server, "active_viewers": r.viewers }))
            .collect()
    } else {
        Vec::new()
    };

    Ok(Json(json!({
        "local_server": local_server,
        "allow_list": allow_list,
        "deny_list": deny_list,
        "federated_joins_total": federated_joins,
        "remote_servers": remote_servers,
    })))
}

/// GET /platform/config-full — redacted full runtime config snapshot.
async fn platform_config_full(
    admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Ok(Json(json!({ "demo": true })));
    }
    // The serde skip_serializing attributes on MonetizationConfig already
    // redact secrets. Everything else is public.
    let cfg = serde_json::to_value(&state.config)
        .map_err(|e| MMError::Internal(format!("config serialize: {e}")))?;
    Ok(Json(cfg))
}

/// GET /platform/deployment — deployment metadata (image, uptime, containers).
async fn platform_deployment(
    _auth: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    Ok(Json(json!({
        "service": "mm-core",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime_secs,
        "started_at": chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(uptime_secs as i64))
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        "monetization": {
            "enabled": state.config.monetization.enabled,
            "donations_enabled": state.config.monetization.donations_enabled,
            "subscriptions_enabled": state.config.monetization.subscriptions_enabled,
            "stripe_api_base": state.config.monetization.stripe_api_base.clone(),
        },
        "features": {
            "redis": !state.config.monetization.redis_url.is_empty(),
            "federation_allow_list_len": state.config.federation.allow_list.len(),
            "federation_deny_list_len": state.config.federation.deny_list.len(),
        },
    })))
}

// ===========================================================================
// Synapse Admin API Proxy
// ===========================================================================
//
// Forwards requests to Synapse's /_synapse/admin/ API using the server-side
// `MM_SYNAPSE_ADMIN_TOKEN`. The browser never sees Synapse credentials.
// All routes are behind AdminAuth — same mm-core admin token as everything else.

fn synapse_client(state: &crate::state::SharedState) -> Result<(reqwest::Client, String, String), ApiError> {
    let token = &state.config.matrix.synapse_admin_token;
    if token.is_empty() {
        return Err(MMError::api(
            ErrorCode::Internal,
            "MM_SYNAPSE_ADMIN_TOKEN not configured",
        ).into());
    }
    let base = state.config.matrix.homeserver_url.clone();
    let client = reqwest::Client::new();
    Ok((client, base, token.clone()))
}

/// GET /synapse/users — list Synapse users (proxy to /_synapse/admin/v2/users)
async fn synapse_list_users(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let (client, base, token) = synapse_client(&state)?;
    let limit = params.get("limit").and_then(|v| v.parse::<u32>().ok()).unwrap_or(200);
    let url = format!("{base}/_synapse/admin/v2/users?limit={limit}");

    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send().await
        .map_err(|e| MMError::Internal(format!("Synapse request failed: {e}")))?;

    let status = resp.status();
    let body: Value = resp.json().await
        .map_err(|e| MMError::Internal(format!("Synapse response parse failed: {e}")))?;

    if !status.is_success() {
        return Err(MMError::Internal(format!(
            "Synapse returned {}: {}",
            status,
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        )).into());
    }

    Ok(Json(body))
}

/// PUT /synapse/users/{user_id} — create or update user
#[derive(Debug, Deserialize)]
struct SynapseUserBody {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    displayname: Option<String>,
    #[serde(default)]
    admin: Option<bool>,
}

async fn synapse_upsert_user(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
    Json(body): Json<SynapseUserBody>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let (client, base, token) = synapse_client(&state)?;
    let encoded = urlencoding::encode(&user_id);
    let url = format!("{base}/_synapse/admin/v2/users/{encoded}");

    let mut payload = serde_json::Map::new();
    if let Some(pw) = &body.password {
        payload.insert("password".into(), json!(pw));
    }
    if let Some(dn) = &body.displayname {
        payload.insert("displayname".into(), json!(dn));
    }
    if let Some(admin) = body.admin {
        payload.insert("admin".into(), json!(admin));
    }

    let resp = client.put(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&payload)
        .send().await
        .map_err(|e| MMError::Internal(format!("Synapse request failed: {e}")))?;

    let status = resp.status();
    let result: Value = resp.json().await
        .map_err(|e| MMError::Internal(format!("Synapse response parse failed: {e}")))?;

    if !status.is_success() {
        return Err(MMError::Internal(format!(
            "Synapse returned {}: {}",
            status,
            result.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        )).into());
    }

    Ok(Json(result))
}

/// POST /synapse/deactivate/{user_id} — deactivate user
async fn synapse_deactivate_user(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let (client, base, token) = synapse_client(&state)?;
    let encoded = urlencoding::encode(&user_id);
    let url = format!("{base}/_synapse/admin/v1/deactivate/{encoded}");

    let resp = client.post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"erase": true}))
        .send().await
        .map_err(|e| MMError::Internal(format!("Synapse request failed: {e}")))?;

    let status = resp.status();
    let result: Value = resp.json().await.unwrap_or(json!({"ok": true}));

    if !status.is_success() {
        return Err(MMError::Internal(format!(
            "Synapse deactivation failed {}: {}",
            status,
            result.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        )).into());
    }

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Advertising admin endpoints (Phase 9)
// ---------------------------------------------------------------------------

/// GET /admin/v1/ads -- List ALL ads (platform + creator) for admin view.
async fn admin_list_ads(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let engine = state.ad_engine.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "advertising disabled"))?;

    let ads = engine.creative_service().list_all(500).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    let list: Vec<serde_json::Value> = ads.iter().map(|a| serde_json::json!({
        "id": a.id,
        "title": a.title,
        "owner_type": a.owner_type,
        "owner_id": a.owner_id,
        "placement": a.placement,
        "duration_secs": a.duration_secs,
        "status": a.status,
        "cdn_url": a.cdn_url,
        "click_through_url": a.click_through_url,
        "mime_type": a.mime_type,
        "file_size_bytes": a.file_size_bytes,
        "categories": a.categories,
        "created_at": a.created_at.to_rfc3339(),
        "updated_at": a.updated_at.to_rfc3339(),
    })).collect();

    Ok(Json(serde_json::json!({ "ads": list })))
}

/// POST /admin/v1/ads -- Upload platform ad.
async fn admin_upload_ad(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let engine = state.ad_engine.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "advertising disabled"))?;

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    let creative = mm_ads::AdCreative {
        id: id.clone(),
        owner_type: "platform".to_string(),
        owner_id: "admin".to_string(),
        title: req["title"].as_str().unwrap_or("Platform Ad").to_string(),
        placement: req["placement"].as_str().unwrap_or("pre_roll").to_string(),
        duration_secs: req["duration_secs"].as_i64().unwrap_or(15) as i32,
        storage_key: format!("ads/{id}.mp4"),
        storage_backend: "local".to_string(),
        cdn_url: req["cdn_url"].as_str().map(String::from),
        mime_type: "video/mp4".to_string(),
        file_size_bytes: 0,
        click_through_url: req["click_through_url"].as_str().map(String::from),
        categories: req.get("categories").cloned().unwrap_or(serde_json::json!([])),
        status: "ready".to_string(),
        created_at: now,
        updated_at: now,
    };

    engine.creative_service().create(&creative).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({ "id": id, "status": "ready" })))
}

/// DELETE /admin/v1/ads/:id -- Delete platform ad.
async fn admin_delete_ad(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let engine = state.ad_engine.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "advertising disabled"))?;

    engine.creative_service().soft_delete(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// PUT /admin/v1/ads/:id -- Update an ad creative.
async fn admin_update_ad(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let engine = state.ad_engine.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "advertising disabled"))?;

    engine.creative_service().update(
        &id,
        req["title"].as_str(),
        req["placement"].as_str(),
        req["status"].as_str(),
        req["click_through_url"].as_str(),
        req.get("categories"),
        req["duration_secs"].as_i64().map(|v| v as i32),
        req["cdn_url"].as_str(),
        None, // file_size
        None, // mime_type
    ).await.map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /admin/v1/ads/:id/upload -- Upload video file for an ad.
/// Accepts multipart form with a `file` field. Saves as WebM. If MP4 input,
/// transcodes to WebM (VP8+Opus) via ffmpeg. Updates DB with cdn_url + duration.
async fn admin_upload_ad_file(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    let engine = state.ad_engine.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "advertising disabled"))?;

    // Read the uploaded file
    let mut file_data = Vec::new();
    let mut file_name = String::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("file") {
            file_name = field.file_name().unwrap_or("upload.mp4").to_string();
            file_data = field.bytes().await
                .map_err(|e| MMError::api(ErrorCode::InvalidAmount, &format!("read error: {e}")))?
                .to_vec();
            break;
        }
    }
    if file_data.is_empty() {
        return Err(MMError::api(ErrorCode::InvalidAmount, "no file in upload").into());
    }

    let ads_dir = "/opt/MatrixMedia/web/ads";
    let is_webm = file_name.ends_with(".webm");
    let input_path = format!("{ads_dir}/{id}_upload{}", if is_webm { ".webm" } else { ".mp4" });
    let output_path = format!("{ads_dir}/{id}.webm");

    // Write uploaded file to disk
    tokio::fs::create_dir_all(ads_dir).await.ok();
    tokio::fs::write(&input_path, &file_data).await
        .map_err(|e| MMError::api(ErrorCode::InvalidAmount, &format!("write error: {e}")))?;

    if is_webm {
        // Already WebM — just rename
        tokio::fs::rename(&input_path, &output_path).await
            .map_err(|e| MMError::api(ErrorCode::InvalidAmount, &format!("rename error: {e}")))?;
    } else {
        // Transcode MP4 → WebM via ffmpeg
        let status = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-i", &input_path,
                   "-c:v", "libvpx", "-b:v", "1M", "-g", "24",
                   "-c:a", "libopus", "-b:a", "64k", "-ac", "2",
                   &output_path])
            .status()
            .await
            .map_err(|e| MMError::api(ErrorCode::InvalidAmount, &format!("ffmpeg error: {e}")))?;
        // Clean up input
        tokio::fs::remove_file(&input_path).await.ok();
        if !status.success() {
            return Err(MMError::api(ErrorCode::InvalidAmount, "ffmpeg transcoding failed").into());
        }
    }

    // Probe duration from the final WebM
    let internal_url = format!("http://mm-web/_mm/ads/{id}.webm");
    let duration = mm_ads::media_probe::probe_duration(&internal_url).await.unwrap_or(30);
    let file_size = tokio::fs::metadata(&output_path).await.map(|m| m.len() as i64).unwrap_or(0);

    // Store the internal HTTP URL that mm-switch can fetch from nginx.
    // mm-switch runs inside Docker and resolves "mm-web" to the nginx container.
    let internal_cdn_url = format!("http://mm-web/_mm/ads/{id}.webm");

    // Update DB
    engine.creative_service().update(
        &id, None, None, Some("ready"), None, None,
        Some(duration),
        Some(&internal_cdn_url),
        Some(file_size),
        Some("video/webm"),
    ).await.map_err(|e| MMError::Database(e.to_string()))?;

    tracing::info!(ad_id = %id, duration, file_size, cdn_url = %internal_cdn_url, "Ad file uploaded and processed");

    Ok(Json(serde_json::json!({
        "ok": true,
        "cdn_url": internal_cdn_url,
        "duration_secs": duration,
        "file_size_bytes": file_size,
    })))
}

/// GET /admin/v1/ads/:id/stats -- Per-ad statistics.
async fn admin_get_ad_stats(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.pg_pool.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "no database"))?;

    let impressions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE ad_id = $1"
    ).bind(&id).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let completions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE ad_id = $1 AND completed_at IS NOT NULL"
    ).bind(&id).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let skips: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE ad_id = $1 AND skipped_at IS NOT NULL"
    ).bind(&id).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let clicks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE ad_id = $1 AND clicked_at IS NOT NULL"
    ).bind(&id).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let completion_rate = if impressions > 0 { completions as f64 / impressions as f64 } else { 0.0 };
    let ctr = if impressions > 0 { clicks as f64 / impressions as f64 } else { 0.0 };

    Ok(Json(serde_json::json!({
        "ad_id": id,
        "total_impressions": impressions,
        "completions": completions,
        "skips": skips,
        "clicks": clicks,
        "completion_rate": (completion_rate * 100.0).round() / 100.0,
        "ctr": (ctr * 100.0).round() / 100.0,
    })))
}

/// GET /admin/v1/ads/analytics -- Platform-wide ad analytics.
async fn admin_ad_analytics(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = state.pg_pool.as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "no database"))?;

    let total_impressions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions"
    ).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let total_completions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE completed_at IS NOT NULL"
    ).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let total_skips: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE skipped_at IS NOT NULL"
    ).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let total_clicks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_impressions WHERE clicked_at IS NOT NULL"
    ).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let total_ads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_ad_creatives WHERE status != 'deleted'"
    ).fetch_one(pool).await.map_err(|e| MMError::Database(e.to_string()))?;

    let completion_rate = if total_impressions > 0 {
        total_completions as f64 / total_impressions as f64
    } else { 0.0 };

    Ok(Json(serde_json::json!({
        "total_ads": total_ads,
        "total_impressions": total_impressions,
        "total_completions": total_completions,
        "total_skips": total_skips,
        "total_clicks": total_clicks,
        "completion_rate": completion_rate,
        "ctr": if total_impressions > 0 { total_clicks as f64 / total_impressions as f64 } else { 0.0 },
    })))
}

// ===========================================================================
// Dashboard Auth Endpoints
// ===========================================================================

/// GET /auth-info -- Public endpoint returning homeserver info for login.
///
/// No authentication required. The dashboard uses this to discover the
/// homeserver URL and server name before initiating the OpenID login flow.
async fn auth_info(
    State(state): State<SharedState>,
) -> Json<Value> {
    // Return PUBLIC homeserver URL for browser-side Matrix login.
    // The internal URL (http://synapse:8008) isn't reachable from browsers.
    // Use the public_url (which includes the correct hostname) or derive
    // from server_name with "matrix." prefix (standard convention).
    let server_name = &state.config.matrix.server_name;
    let public_hs_url = state.config.server.public_url
        .as_deref()
        .map(|u| u.to_string())
        .unwrap_or_else(|| format!("https://matrix.{server_name}"));
    Json(json!({
        "homeserver_url": public_hs_url,
        "server_name": server_name,
    }))
}

#[derive(Debug, Deserialize)]
struct AdminLoginRequest {
    /// Matrix user ID (@user:server) or just the localpart (user)
    user_id: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct AdminLoginResponse {
    token: String,
    role: String,
    user_id: String,
}

/// POST /login -- Server-side Matrix login.
///
/// The browser sends username+password to mm-core. mm-core authenticates
/// against Synapse via the INTERNAL Docker network — no Matrix API is
/// exposed to the browser. Flow:
/// 1. Login to Synapse via CS API (internal URL) → get access_token
/// 2. Request OpenID token from Synapse (internal)
/// 3. Validate OpenID token to confirm user_id
/// 4. Check Synapse admin status
/// 5. Logout the Matrix session (cleanup)
/// 6. Issue MM admin JWT with role
async fn admin_login(
    State(state): State<SharedState>,
    Json(req): Json<AdminLoginRequest>,
) -> Result<Json<AdminLoginResponse>, ApiError> {
    let hs_url = &state.config.matrix.homeserver_url; // internal: http://synapse:8008
    let http = reqwest::Client::new();

    // Ensure user_id has the full @user:server format
    let user_id = if req.user_id.starts_with('@') {
        req.user_id.clone()
    } else {
        format!("@{}:{}", req.user_id, state.config.matrix.server_name)
    };

    // Step 1: Login to Synapse (server-side, internal network)
    let login_resp = http
        .post(format!("{hs_url}/_matrix/client/v3/login"))
        .json(&serde_json::json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": user_id },
            "password": req.password,
        }))
        .send()
        .await
        .map_err(|e| MMError::api(ErrorCode::Forbidden, format!("homeserver unreachable: {e}")))?;

    if !login_resp.status().is_success() {
        let body: serde_json::Value = login_resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("invalid credentials");
        return Err(MMError::api(ErrorCode::Forbidden, msg).into());
    }

    let login_data: serde_json::Value = login_resp.json().await
        .map_err(|e| MMError::api(ErrorCode::Forbidden, format!("login parse error: {e}")))?;
    let access_token = login_data["access_token"].as_str()
        .ok_or_else(|| MMError::api(ErrorCode::Forbidden, "no access_token in login response"))?;
    let confirmed_user_id = login_data["user_id"].as_str().unwrap_or(&user_id).to_string();

    // Step 2: Check if user is a Synapse admin
    let is_admin = check_synapse_admin(&state, &confirmed_user_id).await.unwrap_or(false);
    let role = if is_admin { "admin" } else { "demo" };

    // Step 3: Logout the Matrix session (cleanup — we only needed it for auth)
    let _ = http
        .post(format!("{hs_url}/_matrix/client/v3/logout"))
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await;

    // Step 4: Issue MM admin JWT
    let token = issue_admin_session_token(&confirmed_user_id, role, &state.config.jwt_signing_key)
        .map_err(|e| MMError::Internal(format!("failed to issue admin token: {e}")))?;

    tracing::info!(user = %confirmed_user_id, role, "Dashboard login");

    Ok(Json(AdminLoginResponse {
        token,
        role: role.to_string(),
        user_id: confirmed_user_id,
    }))
}

/// Check whether a Matrix user is a Synapse server admin.
///
/// Calls `GET {homeserver_url}/_synapse/admin/v2/users/{user_id}` using the
/// server-side `synapse_admin_token`. Returns `true` if the user exists and
/// has `admin: true`.
async fn check_synapse_admin(state: &SharedState, user_id: &str) -> Result<bool, MMError> {
    let token = &state.config.matrix.synapse_admin_token;
    if token.is_empty() {
        // No Synapse admin token configured -- cannot check, assume not admin.
        tracing::warn!("MM_SYNAPSE_ADMIN_TOKEN not configured; treating user as non-admin");
        return Ok(false);
    }

    let base = &state.config.matrix.homeserver_url;
    let encoded = urlencoding::encode(user_id);
    let url = format!("{base}/_synapse/admin/v2/users/{encoded}");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse admin check failed: {e}")))?;

    if !resp.status().is_success() {
        // User might not exist or the token is invalid -- treat as not admin.
        return Ok(false);
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse admin response parse failed: {e}")))?;

    Ok(body.get("admin").and_then(|v| v.as_bool()).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// System Health
// ---------------------------------------------------------------------------

/// GET /system-health -- Aggregated system health across all components.
///
/// Requires AdminAuth. Returns health status for mm-core, mm-switch, and
/// database pool statistics.
async fn system_health(
    _admin: AdminAuth, // safe for all roles
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    // Check mm-core components (DB, homeserver, SFU).
    let db_start = std::time::Instant::now();
    let db_ok = state.db.health_check().await.is_ok();
    let db_latency_ms = db_start.elapsed().as_millis() as u64;

    let hs_start = std::time::Instant::now();
    let hs_ok = state.hs_client.whoami().await.is_ok();
    let hs_latency_ms = hs_start.elapsed().as_millis() as u64;

    let sfu_start = std::time::Instant::now();
    let sfu_ok = state.sfu.health_check().await.is_ok();
    let sfu_latency_ms = sfu_start.elapsed().as_millis() as u64;

    // Check mm-switch health (if configured).
    let switch_health = if let Some(ref sc) = state.switch_client {
        match sc.health().await {
            Ok(true) => Some(json!({ "status": "ok" })),
            Ok(false) => Some(json!({ "status": "degraded" })),
            Err(e) => Some(json!({ "status": "error", "error": e })),
        }
    } else {
        None
    };

    // PG pool statistics (if configured).
    let pg_pool_stats = state.pg_pool.as_ref().map(|p| {
        json!({
            "size": p.size(),
            "idle": p.num_idle(),
        })
    });

    let overall = if db_ok && hs_ok && sfu_ok { "ok" } else { "degraded" };

    Ok(Json(json!({
        "status": overall,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "components": {
            "database": {
                "status": if db_ok { "ok" } else { "error" },
                "latency_ms": db_latency_ms,
            },
            "homeserver": {
                "status": if hs_ok { "ok" } else { "error" },
                "latency_ms": hs_latency_ms,
            },
            "sfu": {
                "status": if sfu_ok { "ok" } else { "error" },
                "latency_ms": sfu_latency_ms,
            },
            "switch": switch_health,
            "pg_pool": pg_pool_stats,
        },
    })))
}
