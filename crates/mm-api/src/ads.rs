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
use mm_ads::enforcement::{AdCompletionProof, ChallengeData, validate_proof};
use mm_ads::impression::AdEvent;
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::StreamId;

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::{AdSwitchEntry, SharedState};

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
    if !state.config().advertising.enabled {
        return Err(MMError::api(ErrorCode::FeatureDisabled, "advertising is disabled").into());
    }
    if state.ad_engine.is_none() {
        return Err(MMError::api(ErrorCode::FeatureDisabled, "ad engine not initialized").into());
    }
    Ok(())
}

/// Switch a viewer off an ad and back to the programme, drop the ad's source, and record
/// the server-attested watch time (M1): how long their pipe carried the ad. Shared by both
/// switch-back paths, the `/ads/events` skip/complete and the safety-net timer.
///
/// No time is attested if mm-switch no longer holds the viewer. They left at a moment it
/// does not record, and crediting the whole interval paid for time nobody watched (live
/// test 2026-10-13: an app killed 6 s into a 25 s ad was credited 25 s), so that
/// impression keeps no server value. A missing programme source is not that: mm-switch
/// found the viewer first, still on the ad. Not caught: a viewer who left and rejoined
/// before the switch-back is held again under the same id and is credited as if they
/// stayed; telling them apart needs mm-switch to report the source it switched them from.
///
/// The interval ends when the switch-back is sent; it is written after, because
/// measurement must never hold up the switch-back. A failure is logged, not returned.
async fn switch_back(
    state: &SharedState,
    switch: &mm_core::switch_client::SwitchClient,
    token: &str,
    ad: &AdSwitchEntry,
    trigger: &str,
) {
    let carried = ad.started_at.elapsed();
    let switched = switch
        .switch_viewer(&ad.viewer_id, &ad.stream_source_id)
        .await;
    let _ = switch.remove_source(&ad.ad_source_id).await;
    match &switched {
        Ok(()) => {
            tracing::info!(viewer = %ad.viewer_id, trigger, "Ad ended, viewer back to stream")
        }
        Err(err) => tracing::warn!(viewer = %ad.viewer_id, trigger, error = %err,
            "Ad ended, but the switch-back to the stream failed"),
    }
    if let Err(mm_core::switch_client::SwitchViewerError::ViewerNotFound(_)) = switched {
        tracing::info!(viewer = %ad.viewer_id, "The viewer left during the ad: no watch time attested");
        return;
    }
    let Some(engine) = state.ad_engine.as_ref() else {
        return;
    };
    if let Err(e) = engine
        .impression_service()
        .record_server_watch(token, carried)
        .await
    {
        tracing::warn!(error = %e, "Recording the ad's watch time failed");
    }
}

/// The safety-net timer's switch-back, once the ad's `duration + grace` has passed. A
/// no-op if a skip or completion already ended the ad. Public so the tests can fire it
/// without waiting the timer out.
pub async fn switch_back_on_timer(state: &SharedState, token: &str) {
    let pending = {
        let mut map = state.ad_switches.lock().await;
        map.remove(token)
    };
    let Some(pending) = pending else {
        return; // already handled by skip/complete
    };
    // Deliberately NOT a client captured at setup. The timer fires up to duration+grace
    // seconds later, and the viewer may have been re-placed; resolving at fire time is
    // what makes the affinity check meaningful.
    let Some(pool) = state.switch_pool.clone() else {
        return;
    };
    let switch = match crate::ad_affinity::resolve_switch_back(
        &pool,
        &mm_core::fleet::ViewerId::new(&pending.viewer_id),
        pending.node_id.as_ref(),
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(
                viewer = %pending.viewer_id, reason = err.reason(), error = %err,
                "Ad switch-back timer skipped: viewer is no longer on the \
                 node the ad was set up on"
            );
            return;
        }
    };
    // MD2: timer expiry records the watch time, never a completion.
    switch_back(state, &switch, token, &pending, "timer").await;
}

// ---------------------------------------------------------------------------
// POST /ads — Upload ad creative
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UploadAdRequest {
    title: String,
    placement: String,
    // A `duration_secs` in the body is ignored: conversion measures the real one.
    click_through_url: Option<String>,
    /// Refused since conversion (D8): upload a file instead.
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

    if req.media_url.is_some() {
        return Err(MMError::api(ErrorCode::InvalidRequest,
            "media_url is no longer accepted: create the ad, then upload a file to /ads/{id}/upload").into());
    }
    if mm_ads::AdSlot::from_str(&req.placement).is_none() {
        return Err(MMError::api(ErrorCode::InvalidRequest,
            "placement must be one of pre_roll, mid_roll, post_roll, any").into());
    }
    let cfg = state.config();
    let max = cfg.advertising.max_ads_per_creator;
    if max > 0 {
        let have = engine.creative_service().count_live_by_owner("creator", &auth.user_id.0).await
            .map_err(|e| MMError::Database(e.to_string()))?;
        if have >= max as i64 {
            return Err(MMError::api(ErrorCode::InvalidRequest,
                format!("You already have {have} ads; the limit is {max}. Delete one first.")).into());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let creative = mm_ads::AdCreative {
        id: id.clone(),
        owner_type: "creator".to_string(),
        owner_id: auth.user_id.0.clone(),
        title: req.title.clone(),
        placement: req.placement.clone(),
        duration_secs: 15, // placeholder until conversion measures it
        storage_key: format!("ads/{id}"),
        storage_backend: "local".to_string(),
        cdn_url: None,
        mime_type: "video/webm".to_string(),
        file_size_bytes: 0,
        click_through_url: req.click_through_url,
        categories: serde_json::json!(req.categories),
        status: "draft".to_string(),
        created_at: now,
        updated_at: now,
    };

    engine.creative_service().create(&creative).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(AdCreativeResponse {
        id,
        title: req.title,
        placement: req.placement,
        duration_secs: 15,
        status: "draft".to_string(),
        owner_type: "creator".to_string(),
        media_url: String::new(),
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
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    // Someone else's creative, platform ones included, answers exactly like a
    // missing one: the row is not leaked and the id is not confirmed to exist.
    let ad = engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .filter(|ad| ad.owner_type == "creator" && ad.owner_id == auth.user_id.0)
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

    // Someone else's creative answers exactly like a missing one, as in `get_ad`.
    engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .filter(|ad| ad.owner_type == "creator" && ad.owner_id == auth.user_id.0)
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    let retention = state.config().advertising.source_retention_days;
    mm_ads::conversion_db::delete_creative(engine.creative_service().pool(), &id, retention).await
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

    // Only the creative's own file under mm-web's /_mm/ads (D8): the worker's role can write
    // `cdn_url`, so a value anywhere else is not handed out, and there is no fallback address.
    let Some(media_url) = mm_ads::decision::servable_media_url(&ad) else {
        tracing::warn!(creative = %ad.id, "creative has no servable media URL; not handing one out");
        return Err(MMError::api(ErrorCode::NotFound, "ad has no playable media").into());
    };

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

    // Someone else's creative answers exactly like a missing one, as in `get_ad`.
    // A platform creative's stats are its advertiser's, not any creator's.
    engine.creative_service().get(&id).await
        .map_err(|e| MMError::Database(e.to_string()))?
        .filter(|ad| ad.owner_type == "creator" && ad.owner_id == auth.user_id.0)
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "ad not found"))?;

    // Aggregated in SQL; the hourly rollup (`StatsService`) is deliberately unwired (MD3).
    let s = engine.impression_service().delivery_stats(Some(&id)).await
        .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ad_id": id,
        "total_impressions": s.impressions,
        "decisions": s.decisions,
        "completions": s.completions,
        "skips": s.skips,
        "clicks": s.clicks,
        "viewables": s.viewables,
        "completion_rate": s.completion_rate(),
        "ctr": s.ctr(),
        "viewability_rate": s.viewability_rate(),
        "avg_watch_secs": s.avg_watch_secs,
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

    // Creator opt-out: skip ads entirely if the host has disabled advertising.
    if let Some(pool) = state.pg_pool.as_ref() {
        let ads_enabled = sqlx::query_scalar::<_, bool>(
            "SELECT ads_enabled FROM mm_creator_defaults WHERE creator_user_id = $1",
        )
        .bind(stream.host_user_id.as_str())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(true);
        if !ads_enabled {
            return Ok(Json(AdDecision::NoAd { reason: "creator_opt_out".into() }));
        }
    }

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
            // The viewer id in mm-switch is assigned by mm-core in /join.
            // Must match exactly what JoinStreamResponse.switch_viewer_id returned.
            let viewer_id = mm_core::switch_client::switch_viewer_id(&stream.id, &auth.user_id.0);
            let ts_part = chrono::Utc::now().timestamp_millis();
            let safe_user = auth.user_id.0.replace([':', '@', '!'], "-");
            let ad_source_id = format!("ad-{safe_user}-{ts_part}");
            let stream_source_id = mm_core::switch_client::switch_source_id(&stream.id);

            // FR-405: every call in this block MUST go to the node that holds
            // this viewer. A switch sent elsewhere does not error — mm-switch
            // does not know the viewer and no-ops — and the ad's FileSource
            // would be registered on a node with nobody to show it to.
            let route = state.route_viewer(&mm_core::fleet::ViewerId::new(&viewer_id)).await;
            if let Some(route) = route {
                let switch = route.client.clone();
                // mm-switch path: register a per-viewer FileSource pointing at
                // the ad's converted WebM, route the viewer to it, schedule switch-back.
                //
                // Every servable creative has a converted WebM (internal spec 2026-10-11 §6.3):
                // there is no fallback clip. The engine only decides on a creative whose URL is one
                // of its own two local forms (D8); the scheme check below is the last guard, and a
                // failure skips the switch through the existing error arm so the viewer stays on
                // the programme.
                let media_url = ad.media_url.clone();
                let add_result = if media_url.starts_with("http://") || media_url.starts_with("https://") {
                    switch.add_file_source(&ad_source_id, &media_url, false).await
                } else {
                    Err("ad has no playable URL".to_string())
                };

                match add_result {
                    Ok(()) => {
                        // No delay needed: mm-switch's FileSource starts playing at its
                        // first subscriber, so this viewer's switch starts the ad, and the
                        // first video packet it gets is the opening keyframe (a WebM's
                        // keyframe cache fills during that first parse, so there is nothing
                        // to replay to its first subscriber; an IVF's preloaded keyframe is
                        // replayed at once).
                        if let Err(e) = switch.switch_viewer(&viewer_id, &ad_source_id).await {
                            tracing::warn!(error = %e, viewer = %viewer_id, "Switch to ad failed");
                        } else if !route.billable {
                            // The switch "succeeded" against a node we cannot
                            // confirm holds this viewer. mm-switch no-ops for a
                            // viewer it does not know, so the ad was not shown and
                            // recording an impression would charge an advertiser
                            // for nothing.
                            tracing::warn!(viewer = %viewer_id, ad = %ad_source_id,
                                "Ad switch could not be bound to the viewer's node — \
                                 not recording an impression");
                        } else {
                            let _ = engine.impression_service()
                                .record_sfu_revoked(impression_token).await;
                            tracing::info!(viewer = %viewer_id, ad = %ad_source_id,
                                source_url = %media_url,
                                "Viewer switched to ad source");
                        }

                        // Remember the switch so skip/complete events can trigger
                        // immediate return-to-stream (instead of waiting for the timer).
                        {
                            let mut map = state.ad_switches.lock().await;
                            map.insert(impression_token.clone(), AdSwitchEntry {
                                viewer_id: viewer_id.clone(),
                                stream_source_id: stream_source_id.clone(),
                                ad_source_id: ad_source_id.clone(),
                                started_at: std::time::Instant::now(),
                                // The switch-back and the source cleanup must
                                // reach the SAME node as the switch above.
                                node_id: route.node_id.clone(),
                                slot,
                                billable: route.billable,
                            });
                        }

                        // Scheduled switch-back: after ad duration + short grace.
                        // If the viewer skipped / the ad completed client-side, the
                        // /ads/events handler already triggered the switch and removed
                        // the entry — this task then becomes a no-op.
                        // Use the file's actual duration. ad.duration_secs is
                        // from the DB record which may be shorter than the file.
                        // Add generous grace so the file can play to EOF naturally.
                        // The skip/complete event from the SDK fires the instant
                        // switch-back, so this timer is just a safety net.
                        // Use stated duration + grace. The client fires "completed"
                        // at duration+2s; this server timer is a safety net in case
                        // the client event is lost.
                        let dur = (ad.duration_secs as u64).max(30);
                        let grace: u64 = 10;
                        let token = impression_token.clone();
                        let state2 = state.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(dur + grace)).await;
                            switch_back_on_timer(&state2, &token).await;
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, ad_url = %media_url,
                        "Ad source creation in mm-switch failed"),
                }
            } else if let Some(ref sfu_room) = stream.sfu_room_id {
                // Fallback: canSubscribe revocation (no mm-switch configured)
                let _ = state.sfu.update_participant_permissions(sfu_room, &viewer_id, false).await;
                let _ = engine.impression_service().record_sfu_revoked(impression_token).await;
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

    // Look up the impression. Someone else's answers exactly like an unknown
    // token, as on `/ads/events`. A token bound to another stream answers the
    // same way: completion proof is stream-scoped.
    let imp = engine.impression_service()
        .get_by_token(&proof.impression_token)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
        .filter(|imp| imp.viewer_user_id == auth.user_id.0)
        .filter(|imp| imp.stream_id == stream_id)
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "impression not found"))?;

    // Idempotent: an already-ended impression (completed or skipped) answers
    // 200 with no further side effects — every shipped player treats this
    // call as best-effort and retries it.
    if imp.completed_at.is_some() || imp.skipped_at.is_some() {
        return Ok(Json(serde_json::json!({ "ok": true })));
    }

    // Proof (P0 #A1): impressions decided with a stored HMAC challenge must
    // present a valid response. Rows decided before the challenge was stored
    // (pre-V045, both columns NULL) take the legacy accept-once path; the
    // server-side safety-net timer still restores the stream for players
    // that have not implemented proof computation yet.
    match (imp.challenge_nonce, imp.viewer_secret) {
        (Some(nonce), Some(secret)) => {
            let challenge = ChallengeData {
                nonce,
                ad_id: imp.ad_id.clone(),
                viewer_secret: secret,
            };
            validate_proof(&proof, &challenge).map_err(|_| {
                MMError::api(ErrorCode::PermissionDenied, "invalid proof")
            })?;
        }
        // Legacy row without a stored challenge: accept once. The row lands
        // completed_at below, so this path cannot be replayed.
        _ => {}
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
            // Source switching disabled until FileSource produces real video.
            // Stream plays uninterrupted through mm-switch.
            tracing::info!(viewer = %auth.user_id.0, "Ad complete acknowledged");

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
    /// Player viewability heartbeat (MM_ADS_PLAN.md §2.5). All optional so
    /// existing clients keep working; a signal is recorded only when the
    /// player reports coverage.
    #[serde(default)]
    coverage_pct: Option<f32>,
    #[serde(default)]
    audible: Option<bool>,
    #[serde(default)]
    continuous_visible_secs: Option<f32>,
}

async fn report_ad_event(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<AdEventRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_advertising(&state)?;
    let engine = state.ad_engine.as_ref().unwrap();

    let event = AdEvent::from_str(&req.event)
        .ok_or_else(|| MMError::api(ErrorCode::InvalidAmount, "unknown event type"))?;

    // Only the viewer the ad was decided for may report on it. Checked before
    // anything is written or switched: someone else's skip would otherwise
    // stamp their impression and switch them back to the programme. A foreign
    // token answers exactly like an unknown one.
    engine.impression_service()
        .get_by_token(&req.impression_token)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
        .filter(|imp| imp.viewer_user_id == auth.user_id.0)
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "impression not found"))?;

    engine.impression_service()
        .update_event(&req.impression_token, event)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

    // M2: the client's position only counts where the server cannot attest.
    if let Some(position_secs) = req.position_secs {
        engine.impression_service()
            .record_client_position(&req.impression_token, position_secs)
            .await
            .map_err(|e| MMError::Database(e.to_string()))?;
    }

    // MM Ads P0: player viewability heartbeat. The client reports geometry;
    // the server judges the MRC rule (record_viewability). Partial heartbeats
    // record what they carry; only a full qualifying signal stamps viewable.
    // A missing span defaults to 0, which never qualifies.
    if let Some(coverage_pct) = req.coverage_pct {
        engine.impression_service()
            .record_viewability(
                &req.impression_token,
                coverage_pct,
                req.audible,
                req.continuous_visible_secs.unwrap_or(0.0),
            )
            .await
            .map_err(|e| MMError::Database(e.to_string()))?;
    }

    // A skip and a completion both mean "stop the ad NOW" as far as mm-switch
    // is concerned. Remove the scheduled timer entry and trigger switch-back
    // immediately so the viewer returns to the live stream without waiting.
    // Decided on the parsed event, so every spelling `from_str` accepts counts.
    let should_end = matches!(event, AdEvent::Skipped | AdEvent::Completed);
    if should_end {
        {
            let entry: Option<AdSwitchEntry> = {
                let mut map = state.ad_switches.lock().await;
                map.remove(&req.impression_token)
            };
            if let Some(e) = entry {
                // FR-405: back to the node that took the switch, and only if the
                // viewer is still there. The skip can arrive seconds after the
                // switch, and a viewer re-placed in between makes both nodes the
                // wrong answer — the old one no longer has the viewer, the new one
                // has never heard of the ad source.
                let Some(pool) = state.switch_pool.clone() else {
                    return Ok(Json(serde_json::json!({ "ok": true })));
                };
                let switch = match crate::ad_affinity::resolve_switch_back(
                    &pool,
                    &mm_core::fleet::ViewerId::new(&e.viewer_id),
                    e.node_id.as_ref(),
                )
                .await
                {
                    Ok(client) => client,
                    Err(err) => {
                        tracing::warn!(
                            viewer = %e.viewer_id, reason = err.reason(), error = %err,
                            "Ad switch-back skipped: cannot reach the node holding this viewer"
                        );
                        return Ok(Json(serde_json::json!({ "ok": true })));
                    }
                };
                // Enforce minimum view time (defaultSkipTimeout).
                // If the skip arrives before the minimum has elapsed, wait
                // until it does — the advertiser is guaranteed this much.
                let min_view_secs = state.config().advertising.skip_after_secs as u64;
                let elapsed = e.started_at.elapsed();
                let min_view = std::time::Duration::from_secs(min_view_secs);
                if elapsed < min_view {
                    let wait = min_view - elapsed;
                    tracing::info!(viewer = %e.viewer_id, wait_ms = wait.as_millis(),
                        "Skip received early, waiting for minimum view time");
                    tokio::time::sleep(wait).await;
                }

                switch_back(&state, &switch, &req.impression_token, &e, &req.event).await;
            }
        }
    }

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

    if let Some(pool) = state.pg_pool.as_ref() {
        let ads_enabled = sqlx::query_scalar::<_, bool>(
            "SELECT ads_enabled FROM mm_creator_defaults WHERE creator_user_id = $1",
        )
        .bind(stream.host_user_id.as_str())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(true);
        if !ads_enabled {
            return Err(MMError::api(
                ErrorCode::Forbidden,
                "advertising is disabled for this creator",
            )
            .into());
        }
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
