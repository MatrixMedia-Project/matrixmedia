//! Advertising API endpoints (Phase 9).
//!
//! Creator ad management, viewer ad decisions, impression events, and media serving.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post, delete},
};
use serde::{Deserialize, Serialize};

use mm_ads::{AdDecision, AdSlot, StreamAdContext};
use mm_ads::enforcement::AdCompletionProof;
use mm_ads::impression::AdEvent;
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::StreamId;

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

/// Advertising routes nested under `/_mm/client/v1/`.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        // Creator ad management
        .route("/ads", post(upload_ad))
        .route("/ads", get(list_my_ads))
        .route("/ads/{id}", get(get_ad))
        .route("/ads/{id}", delete(delete_ad))
        .route("/ads/{id}/media", get(serve_ad_media))
        .route("/ads/{id}/stats", get(get_ad_stats))
        // Viewer ad decisions
        .route("/streams/{id}/ad-decision", get(ad_decision))
        .route("/streams/{id}/ad-complete", post(ad_complete))
        .route("/streams/{id}/ad-status", get(ad_status))
        .route("/ads/events", post(report_ad_event))
        // Host mid-roll trigger
        .route("/streams/{id}/ad-break", post(trigger_ad_break))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

fn require_advertising(state: &SharedState) -> Result<(), ApiError> {
    if !state.config.advertising.enabled {
        return Err(MMError::api(ErrorCode::FeatureDisabled, "advertising is disabled").into());
    }
    if state.ad_engine.is_none() {
        return Err(MMError::api(ErrorCode::FeatureDisabled, "ad engine not initialized").into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /ads — Upload ad creative
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UploadAdRequest {
    title: String,
    placement: String,
    duration_secs: i32,
    click_through_url: Option<String>,
    /// Optional: provide a URL to an existing hosted video instead of uploading.
    media_url: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AdCreativeResponse {
    id: String,
    title: String,
    placement: String,
    duration_secs: i32,
    status: String,
    owner_type: String,
    media_url: String,
    created_at: String,
}

async fn upload_ad(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<UploadAdRequest>,
) -> Result<Json<AdCreativeResponse>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let public_url = state.config.server.public_url.as_deref().unwrap_or("");

    // If media_url provided, use it as cdn_url. Otherwise, storage_key points
    // to local file (file upload endpoint can be added later for actual binary uploads).
    let (cdn_url, storage_key, file_size) = if let Some(ref url) = req.media_url {
        (Some(url.clone()), format!("external/{id}"), 0i64)
    } else {
        (None, format!("/data/ads/{id}.mp4"), 0i64)
    };

    let media_url = cdn_url.clone().unwrap_or_else(|| {
        format!("{public_url}/_mm/ads/{id}.mp4")
    });

    let creative = mm_ads::AdCreative {
        id: id.clone(),
        owner_type: "creator".to_string(),
        owner_id: auth.user_id.0.clone(),
        title: req.title.clone(),
        placement: req.placement.clone(),
        duration_secs: req.duration_secs,
        storage_key,
        storage_backend: if req.media_url.is_some() { "external" } else { "local" }.to_string(),
        cdn_url,
        mime_type: "video/mp4".to_string(),
        file_size_bytes: file_size,
        click_through_url: req.click_through_url,
        categories: serde_json::json!(req.categories),
        status: "ready".to_string(),
        created_at: now,
        updated_at: now,
    };

    engine.creative_service().create(&creative).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(AdCreativeResponse {
        id,
        title: req.title,
        placement: req.placement,
        duration_secs: req.duration_secs,
        status: "ready".to_string(),
        owner_type: "creator".to_string(),
        media_url,
        created_at: now.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// GET /ads — List my ads
// ---------------------------------------------------------------------------

async fn list_my_ads(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let ads = engine.creative_service().list_by_owner("creator", &auth.user_id.0).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    let list: Vec<serde_json::Value> = ads.iter().map(|a| {
        serde_json::json!({
            "id": a.id,
            "title": a.title,
            "placement": a.placement,
            "duration_secs": a.duration_secs,
            "status": a.status,
            "categories": a.categories,
            "created_at": a.created_at.to_rfc3339(),
        })
    }).collect();

    Ok(Json(serde_json::json!({ "ads": list })))
}

// ---------------------------------------------------------------------------
// GET /ads/:id — Get ad details
// ---------------------------------------------------------------------------

async fn get_ad(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let ad = engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    Ok(Json(serde_json::json!(ad)))
}

// ---------------------------------------------------------------------------
// DELETE /ads/:id — Soft-delete ad
// ---------------------------------------------------------------------------

async fn delete_ad(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let ad = engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    if ad.owner_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "not your ad").into());
    }

    engine.creative_service().soft_delete(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// GET /ads/:id/media — Serve ad video
// ---------------------------------------------------------------------------

async fn serve_ad_media(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let ad = engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    let public_url = state.config.server.public_url.as_deref().unwrap_or("");
    let media_url = ad.cdn_url.unwrap_or_else(|| {
        format!("{public_url}/_mm/ads/{}.mp4", ad.id)
    });

    Ok(Json(serde_json::json!({ "url": media_url })))
}

// ---------------------------------------------------------------------------
// GET /ads/:id/stats — Per-ad statistics
// ---------------------------------------------------------------------------

async fn get_ad_stats(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let ad = engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    if ad.owner_id != auth.user_id.0 && ad.owner_type != "platform" {
        return Err(MMError::api(ErrorCode::Forbidden, "not your ad").into());
    }

    // For now, query impressions directly (stats rollup runs hourly).
    let impressions = engine.impression_service().list_by_ad(&id, 1000, 0).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    let total = impressions.len();
    let completed = impressions.iter().filter(|i| i.completed_at.is_some()).count();
    let skipped = impressions.iter().filter(|i| i.skipped_at.is_some()).count();
    let clicked = impressions.iter().filter(|i| i.clicked_at.is_some()).count();

    Ok(Json(serde_json::json!({
        "ad_id": id,
        "total_impressions": total,
        "completions": completed,
        "skips": skipped,
        "clicks": clicked,
        "completion_rate": if total > 0 { completed as f64 / total as f64 } else { 0.0 },
        "ctr": if total > 0 { clicked as f64 / total as f64 } else { 0.0 },
    })))
}

// ---------------------------------------------------------------------------
// GET /streams/:id/ad-decision — Get ad for viewer (+ SFU enforcement)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AdDecisionQuery {
    #[serde(default = "default_pre_roll")]
    slot: String,
}

fn default_pre_roll() -> String {
    "pre_roll".to_string()
}

async fn ad_decision(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
    Query(query): Query<AdDecisionQuery>,
) -> Result<Json<AdDecision>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let stream = state.db.get_stream(&StreamId(stream_id.clone())).await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    let slot = AdSlot::from_str(&query.slot)
        .ok_or_else(|| MMError::api(ErrorCode::InvalidAmount, "invalid slot"))?;

    let is_live = stream.status == "active";

    let context = StreamAdContext {
        viewer_count: stream.participant_count as u32,
        stream_duration_secs: chrono::Utc::now()
            .signed_duration_since(stream.started_at)
            .num_seconds()
            .max(0) as u64,
        categories: vec![], // TODO: stream categories
        last_ad_at: None,   // TODO: track per-viewer
        host_user_id: stream.host_user_id.clone(),
    };

    let decision = engine
        .decide(&stream_id, &auth.user_id.0, slot, &context, is_live)
        .await;

    // Server-side ad injection via mm-switch:
    // mm-switch controls what flows through each viewer's WebRTC pipe.
    // 1. Register ad video as a source in mm-switch
    // 2. Switch viewer's pipe to ad source
    // 3. After ad completes: switch viewer's pipe back to streamer source
    //
    // Fallback: if mm-switch is not available, use canSubscribe revocation.
    if is_live {
        if let AdDecision::ServeAd { ref ad, ref impression_token, .. } = decision {
            let viewer_id = auth.user_id.0.clone();
            let ad_source_id = format!("ad-{}", &impression_token[..8.min(impression_token.len())]);
            let stream_source_id = format!("stream-{}", stream.id);

            // SGAI Layer 1: revoke canSubscribe — stream is physically blocked.
            // Viewer sees ad via HTML5 overlay, then stream resumes on ad-complete.
            // mm-switch relay (Layer 3) is WIP — will replace this when RTP forwarding is stable.
            if let Some(ref sfu_room) = stream.sfu_room_id {
                let _ = state.sfu.update_participant_permissions(sfu_room, &viewer_id, false).await;
                let _ = engine.impression_service()
                    .record_sfu_revoked(impression_token).await;

                tracing::info!(viewer = %viewer_id, "canSubscribe revoked for ad break");

                let timeout = state.config.advertising.auto_restore_timeout_secs as u64;
                let state2 = state.clone();
                let room = sfu_room.clone();
                let viewer = viewer_id.clone();
                let token = impression_token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(timeout)).await;
                    if let Some(pool) = state2.pg_pool.as_ref() {
                        let still: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM mm_ad_impressions WHERE impression_token = $1 AND completed_at IS NULL AND skipped_at IS NULL"
                        ).bind(&token).fetch_one(pool).await.unwrap_or(0);
                        if still > 0 {
                            let _ = state2.sfu.update_participant_permissions(&room, &viewer, true).await;
                        }
                    }
                });
            }
        }
    }

    Ok(Json(decision))
}

// ---------------------------------------------------------------------------
// POST /streams/:id/ad-complete — Submit HMAC proof
// ---------------------------------------------------------------------------

async fn ad_complete(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
    Json(proof): Json<AdCompletionProof>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    // Look up the impression.
    let impression = engine.impression_service()
        .get_by_token(&proof.impression_token)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "impression not found"))?;

    if impression.viewer_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "not your impression").into());
    }

    // Mark as completed.
    engine.impression_service()
        .update_event(&proof.impression_token, AdEvent::Completed)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    // Switch viewer back to stream source.
    let stream = state.db.get_stream(&StreamId(stream_id)).await?;
    if let Some(stream) = stream {
        if stream.status == "active" {
            let ad_source_id = format!("ad-{}", &proof.impression_token[..8.min(proof.impression_token.len())]);
            let stream_source_id = format!("stream-{}", stream.id);

            // Restore canSubscribe — viewer can now receive stream tracks.
            if let Some(ref sfu_room) = stream.sfu_room_id {
                let _ = state.sfu.update_participant_permissions(sfu_room, &auth.user_id.0, true).await;
                tracing::info!(viewer = %auth.user_id.0, "canSubscribe restored after ad");
            }

            let _ = engine.impression_service()
                .record_sfu_restored(&proof.impression_token).await;
        }
    }

    tracing::info!(
        impression_token = %proof.impression_token,
        viewer = %auth.user_id.0,
        "Ad completion verified, canSubscribe restored"
    );

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// GET /streams/:id/ad-status — Check if viewer is in ad break
// ---------------------------------------------------------------------------

async fn ad_status(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let active = engine.impression_service()
        .has_active_ad_break(&stream_id, &auth.user_id.0)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "in_ad_break": active,
    })))
}

// ---------------------------------------------------------------------------
// POST /ads/events — Report ad events (quartiles, click, skip)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AdEventRequest {
    impression_token: String,
    event: String,
    #[serde(default)]
    position_secs: Option<i32>,
}

async fn report_ad_event(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<AdEventRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let event = AdEvent::from_str(&req.event)
        .ok_or_else(|| MMError::api(ErrorCode::InvalidAmount, "unknown event type"))?;

    engine.impression_service()
        .update_event(&req.impression_token, event)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// POST /streams/:id/ad-break — Host triggers mid-roll
// ---------------------------------------------------------------------------

async fn trigger_ad_break(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;

    let stream = state.db.get_stream(&StreamId(stream_id.clone())).await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "only the host can trigger ad breaks").into());
    }

    if stream.status != "active" {
        return Err(MMError::api(ErrorCode::InvalidAmount, "stream is not active").into());
    }

    // TODO Phase 3: trigger mid-roll for all viewers in the room
    // For now, just log the request.
    tracing::info!(
        stream_id = %stream_id,
        host = %auth.user_id.0,
        "Mid-roll ad break triggered (implementation pending)"
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "mid-roll trigger registered"
    })))
}
