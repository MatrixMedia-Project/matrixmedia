//! Content moderation (E3): report intake, operator queue, takedown actions,
//! append-only audit. New tables via `mm_db::moderation_db` (raw PgPool on
//! `state.signup_pool`); Synapse admin calls reuse the Bearer-token pattern.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::middleware::{AdminAuth, AdminRole, AuthUser};
use crate::state::SharedState;
use mm_core::error::{ErrorCode, MMError};

#[derive(Debug, Deserialize)]
pub struct ReportRequest {
    pub target_type: String, // stream | recording | user
    pub target_id: String,
    pub reason: String,
    #[serde(default)]
    pub details: Option<String>,
}

const MM_REPORT_TARGETS: [&str; 3] = ["stream", "recording", "user"];

/// `POST /_mm/client/v1/moderation/report`
///
/// Authenticated reporter submits a report against a stream, recording, or
/// user. Validation is local; persistence is via the raw-PgPool
/// `moderation_db::insert_report`. Returns the new report id (or `null` when
/// the insert was deduplicated by the DB layer).
async fn submit_report(
    State(state): State<SharedState>,
    auth: AuthUser,
    Json(req): Json<ReportRequest>,
) -> Result<Json<Value>, ApiError> {
    if !MM_REPORT_TARGETS.contains(&req.target_type.as_str()) {
        return Err(MMError::api(ErrorCode::InvalidRequest, "invalid target_type").into());
    }
    if req.reason.trim().is_empty() {
        return Err(MMError::api(ErrorCode::InvalidRequest, "reason required").into());
    }

    let reporter = auth.user_id.0;

    // Per-reporter rate limit (keyed on MXID, 10/hr) so cross-IP abuse from
    // the same account still throttles.
    if state.moderation_report_limiter.allow(&reporter).is_err() {
        return Err(MMError::api(ErrorCode::RateLimited, "too many reports").into());
    }

    let id = mm_db::moderation_db::insert_report(
        &state.signup_pool,
        &mm_db::moderation_db::NewReport {
            source: "mm",
            target_type: &req.target_type,
            target_id: &req.target_id,
            room_id: None,
            reported_user_id: if req.target_type == "user" {
                Some(req.target_id.as_str())
            } else {
                None
            },
            reporter_id: Some(&reporter),
            reason: &req.reason,
            details: req.details.as_deref(),
            synapse_report_id: None,
        },
    )
    .await
    .map_err(|e| MMError::Internal(format!("insert_report: {e}")))?;

    Ok(Json(json!({ "ok": true, "id": id })))
}

/// Build the moderation client router (mounted under `/_mm/client/v1`).
pub fn client_routes(state: SharedState) -> Router {
    Router::new()
        .route("/moderation/report", post(submit_report))
        .with_state(state)
}

// ===========================================================================
// Task B1 — Synapse admin helpers
// ===========================================================================

/// Build a `(client, base_url, admin_token)` triple for Synapse admin calls.
///
/// Mirrors `admin::synapse_client`; replicated here (that helper is private to
/// the `admin` module) so the moderation surface stays self-contained.
fn synapse(state: &SharedState) -> Result<(reqwest::Client, String, String), ApiError> {
    let token = &state.config.matrix.synapse_admin_token;
    if token.is_empty() {
        return Err(MMError::api(ErrorCode::Internal, "MM_SYNAPSE_ADMIN_TOKEN not configured").into());
    }
    let base = state.config.matrix.homeserver_url.clone();
    Ok((reqwest::Client::new(), base, token.clone()))
}

/// `PUT /_synapse/admin/v1/suspend/{user}` — suspend or un-suspend a user.
///
/// Kept `pub` so the action handler (and future tests / background tasks) can
/// drive Synapse suspension directly.
pub async fn synapse_set_suspended(
    state: &SharedState,
    user_id: &str,
    suspend: bool,
) -> Result<(), ApiError> {
    let (client, base, token) = synapse(state)?;
    let encoded = urlencoding::encode(user_id);
    let url = format!("{base}/_synapse/admin/v1/suspend/{encoded}");

    let resp = client
        .put(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "suspend": suspend }))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse suspend request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(json!({}));
        return Err(MMError::Internal(format!(
            "Synapse suspend failed {}: {}",
            status,
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        ))
        .into());
    }
    Ok(())
}

/// One report row from Synapse's event-report listing. All fields are lenient
/// (`#[serde(default)]`) so a Synapse-side field rename degrades to None/empty
/// rather than a hard decode failure — runtime field names are verified in the
/// deploy smoke test, not here.
#[derive(Debug, Deserialize)]
struct SynapseEventReport {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    /// Reporter MXID.
    #[serde(default)]
    user_id: Option<String>,
    /// MXID of the sender of the reported content.
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SynapseEventReportPage {
    #[serde(default)]
    event_reports: Vec<SynapseEventReport>,
    #[serde(default)]
    #[allow(dead_code)]
    next_token: Option<i64>,
}

/// `GET /_synapse/admin/v1/event_reports?dir=f&limit={limit}` — fetch the most
/// recent batch of Matrix-native event reports.
async fn fetch_event_reports(
    state: &SharedState,
    limit: u32,
) -> Result<Vec<SynapseEventReport>, ApiError> {
    let (client, base, token) = synapse(state)?;
    let url = format!("{base}/_synapse/admin/v1/event_reports?dir=f&limit={limit}");

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse event_reports request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(json!({}));
        return Err(MMError::Internal(format!(
            "Synapse event_reports failed {}: {}",
            status,
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        ))
        .into());
    }

    let page: SynapseEventReportPage = resp
        .json()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse event_reports parse failed: {e}")))?;
    Ok(page.event_reports)
}

// ===========================================================================
// Operator authorization + identity
// ===========================================================================

/// Reject demo admins; full admins pass. Copies the demo-rejection idiom used
/// throughout `admin.rs`.
fn require_operator(admin: &AdminAuth) -> Result<(), ApiError> {
    if matches!(admin.role, AdminRole::Demo) {
        return Err(MMError::api(ErrorCode::Forbidden, "admin access required").into());
    }
    Ok(())
}

/// Resolve the operator identity string for audit rows.
///
/// JWT-authenticated admins carry their MXID in `user_id`. The legacy static
/// admin token has no identity, so we derive a stable synthetic operator MXID
/// from the configured `server_name`.
fn operator_id(admin: &AdminAuth, state: &SharedState) -> String {
    admin
        .user_id
        .clone()
        .unwrap_or_else(|| format!("@operator:{}", state.config.matrix.server_name))
}

// ===========================================================================
// Reusable actuators (C4) — extracted core logic from admin.rs handlers
// ===========================================================================

/// Force-stop a live stream. Core logic mirrors `admin::force_stop_stream`
/// (handler-bound there; the body is short, so it is replicated here to keep
/// the moderation action self-contained while reusing the same DB/SFU/Matrix
/// primitives).
async fn actuate_force_stop_stream(state: &SharedState, stream_id: &str) -> Result<(), ApiError> {
    use mm_core::types::{StreamId, StreamStatus};
    use mm_matrix::events;

    let sid = StreamId(stream_id.to_string());
    let stream = state
        .db
        .get_stream(&sid)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    if let Some(ref sfu_room_id) = stream.sfu_room_id {
        let _ = state.sfu.delete_room(sfu_room_id).await;
    }

    state
        .db
        .update_stream_status(&sid, StreamStatus::Ended)
        .await?;

    if let Some(room) = state.db.get_room(stream.room_id).await? {
        // Terminal stream marker via the shared guaranteed-write path
        // (ensure bot + retry + failure metric + E2EE key clear).
        let _ = crate::stream_lifecycle::finalize_stream_marker(
            &crate::stream_lifecycle::MarkerContext::from_state(state),
            &stream,
            &room.matrix_room_id,
        )
        .await;

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
    Ok(())
}

/// Force-delete a recording (storage + status). Core logic mirrors
/// `admin::admin_delete_recording`, reusing `client::delete_recording_storage`.
async fn actuate_delete_recording(state: &SharedState, recording_id: &str) -> Result<(), ApiError> {
    use crate::client::delete_recording_storage;
    use mm_db::models::RecordingStatus;

    let recording = state
        .db
        .get_recording(recording_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "recording not found"))?;

    if recording.status == RecordingStatus::Deleted.as_str() {
        return Err(MMError::api(ErrorCode::NotFound, "recording not found").into());
    }

    delete_recording_storage(state, &recording).await;

    state
        .db
        .update_recording_status(&recording.id, RecordingStatus::Deleted)
        .await?;
    Ok(())
}

/// Deactivate (erase) a Synapse user. Core logic mirrors
/// `admin::synapse_deactivate_user`.
async fn actuate_deactivate_user(state: &SharedState, user_id: &str) -> Result<(), ApiError> {
    let (client, base, token) = synapse(state)?;
    let encoded = urlencoding::encode(user_id);
    let url = format!("{base}/_synapse/admin/v1/deactivate/{encoded}");

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "erase": true }))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("Synapse deactivate request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body: Value = resp.json().await.unwrap_or(json!({}));
        return Err(MMError::Internal(format!(
            "Synapse deactivation failed {}: {}",
            status,
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        ))
        .into());
    }
    Ok(())
}

// ===========================================================================
// Task C3 — operator queue handlers
// ===========================================================================

#[derive(Debug, Deserialize)]
struct ListReportsParams {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /moderation/reports` — paginated operator queue.
async fn list_reports(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Query(params): Query<ListReportsParams>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let offset = params.offset.unwrap_or(0).max(0);
    let status = params.status.as_deref().filter(|s| !s.is_empty());

    let rows = mm_db::moderation_db::list_reports(&state.signup_pool, status, limit, offset)
        .await
        .map_err(|e| MMError::Internal(format!("list_reports: {e}")))?;

    Ok(Json(json!({ "reports": rows })))
}

/// `GET /moderation/reports/{id}` — one report plus its action history.
async fn get_report(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    let report = mm_db::moderation_db::get_report(&state.signup_pool, id)
        .await
        .map_err(|e| MMError::Internal(format!("get_report: {e}")))?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "report not found"))?;

    let actions = mm_db::moderation_db::list_actions_for_report(&state.signup_pool, id)
        .await
        .map_err(|e| MMError::Internal(format!("list_actions_for_report: {e}")))?;

    Ok(Json(json!({ "report": report, "actions": actions })))
}

#[derive(Debug, Deserialize)]
struct UpdateStatusRequest {
    status: String,
    #[serde(default)]
    reason: String,
}

const REPORT_STATUSES: [&str; 3] = ["open", "actioned", "dismissed"];

/// `PUT /moderation/reports/{id}/status` — set a report's resolution state.
async fn update_status(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<UpdateStatusRequest>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    if !REPORT_STATUSES.contains(&req.status.as_str()) {
        return Err(MMError::api(ErrorCode::InvalidRequest, "invalid status").into());
    }
    let op = operator_id(&admin, &state);

    let updated = mm_db::moderation_db::set_report_status(&state.signup_pool, id, &req.status, &op)
        .await
        .map_err(|e| MMError::Internal(format!("set_report_status: {e}")))?;
    if !updated {
        return Err(MMError::api(ErrorCode::NotFound, "report not found").into());
    }

    if req.status == "dismissed" {
        let id_str = id.to_string();
        mm_db::moderation_db::append_action(
            &state.signup_pool,
            &mm_db::moderation_db::NewAction {
                report_id: Some(id),
                action_type: "dismiss_report",
                target_type: "report",
                target_id: &id_str,
                operator_id: &op,
                reason: &req.reason,
                metadata: json!({}),
            },
        )
        .await
        .map_err(|e| MMError::Internal(format!("append_action: {e}")))?;
    }

    Ok(Json(json!({ "ok": true })))
}

/// Shared sync routine: pull Synapse event reports and persist new ones.
///
/// Returns the number of rows actually inserted (deduplicated rows return
/// `None` from `insert_report` and are not counted). Kept `pub` so the future
/// background task can call it directly.
pub async fn run_sync(state: &SharedState) -> Result<u64, ApiError> {
    let reports = fetch_event_reports(state, 100).await?;
    let mut inserted = 0u64;

    for r in reports {
        let target_id = r.event_id.clone().unwrap_or_default();
        let reason = r.reason.clone().unwrap_or_else(|| "(none)".to_string());

        let id = mm_db::moderation_db::insert_report(
            &state.signup_pool,
            &mm_db::moderation_db::NewReport {
                source: "matrix",
                target_type: "event",
                target_id: &target_id,
                room_id: r.room_id.as_deref(),
                reported_user_id: r.sender.as_deref(),
                reporter_id: r.user_id.as_deref(),
                reason: &reason,
                details: None,
                synapse_report_id: Some(r.id),
            },
        )
        .await
        .map_err(|e| MMError::Internal(format!("insert_report: {e}")))?;

        if id.is_some() {
            inserted += 1;
        }
    }

    Ok(inserted)
}

/// `POST /moderation/reports/sync` — pull Matrix-native event reports.
async fn sync_reports(
    admin: AdminAuth,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;
    let inserted = run_sync(&state).await?;
    Ok(Json(json!({ "ok": true, "inserted": inserted })))
}

// ===========================================================================
// Task C4 — actions, audit, user views
// ===========================================================================

#[derive(Debug, Deserialize)]
struct ApplyActionRequest {
    action_type: String,
    target_type: String,
    target_id: String,
    #[serde(default)]
    report_id: Option<uuid::Uuid>,
    #[serde(default)]
    reason: String,
}

/// `POST /moderation/actions` — apply a takedown/moderation action, then record
/// it in the append-only audit log.
async fn apply_action(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Json(req): Json<ApplyActionRequest>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    if req.reason.trim().is_empty() {
        return Err(MMError::api(ErrorCode::InvalidRequest, "reason required").into());
    }
    let op = operator_id(&admin, &state);
    let mut metadata = json!({});

    match req.action_type.as_str() {
        "force_stop_stream" => actuate_force_stop_stream(&state, &req.target_id).await?,
        "hide_recording" => {
            state.db.set_recording_hidden(&req.target_id, true).await?;
        }
        "unhide_recording" => {
            state.db.set_recording_hidden(&req.target_id, false).await?;
        }
        "delete_recording" => actuate_delete_recording(&state, &req.target_id).await?,
        "suspend_user" => {
            synapse_set_suspended(&state, &req.target_id, true).await?;
            mm_db::moderation_db::set_user_suspended(
                &state.signup_pool,
                &req.target_id,
                true,
                &op,
                &req.reason,
            )
            .await
            .map_err(|e| MMError::Internal(format!("set_user_suspended: {e}")))?;
        }
        "unsuspend_user" => {
            synapse_set_suspended(&state, &req.target_id, false).await?;
            mm_db::moderation_db::set_user_suspended(
                &state.signup_pool,
                &req.target_id,
                false,
                &op,
                &req.reason,
            )
            .await
            .map_err(|e| MMError::Internal(format!("set_user_suspended: {e}")))?;
        }
        "deactivate_user" => {
            actuate_deactivate_user(&state, &req.target_id).await?;
            metadata = json!({ "erase": true });
        }
        _ => return Err(MMError::api(ErrorCode::InvalidRequest, "unknown action").into()),
    }

    mm_db::moderation_db::append_action(
        &state.signup_pool,
        &mm_db::moderation_db::NewAction {
            report_id: req.report_id,
            action_type: &req.action_type,
            target_type: &req.target_type,
            target_id: &req.target_id,
            operator_id: &op,
            reason: &req.reason,
            metadata,
        },
    )
    .await
    .map_err(|e| MMError::Internal(format!("append_action: {e}")))?;

    if let Some(rid) = req.report_id {
        let _ = mm_db::moderation_db::set_report_status(&state.signup_pool, rid, "actioned", &op)
            .await
            .map_err(|e| MMError::Internal(format!("set_report_status: {e}")))?;
    }

    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
struct AuditParams {
    target_type: String,
    target_id: String,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /moderation/audit` — append-only action history for a target.
async fn list_audit(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Query(params): Query<AuditParams>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let actions = mm_db::moderation_db::list_actions_for_target(
        &state.signup_pool,
        &params.target_type,
        &params.target_id,
        limit,
    )
    .await
    .map_err(|e| MMError::Internal(format!("list_actions_for_target: {e}")))?;

    Ok(Json(json!({ "actions": actions })))
}

/// `GET /moderation/users/{user_id}` — current moderation state + history.
async fn user_status(
    admin: AdminAuth,
    State(state): State<SharedState>,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_operator(&admin)?;

    let moderation = mm_db::moderation_db::get_user_moderation(&state.signup_pool, &user_id)
        .await
        .map_err(|e| MMError::Internal(format!("get_user_moderation: {e}")))?;

    let actions =
        mm_db::moderation_db::list_actions_for_target(&state.signup_pool, "user", &user_id, 100)
            .await
            .map_err(|e| MMError::Internal(format!("list_actions_for_target: {e}")))?;

    Ok(Json(json!({ "moderation": moderation, "actions": actions })))
}

/// Build the moderation admin router (mounted under `/_mm/admin/v1`).
pub fn admin_routes(state: SharedState) -> Router {
    Router::new()
        .route("/moderation/reports", get(list_reports))
        .route("/moderation/reports/sync", post(sync_reports))
        .route("/moderation/reports/{id}", get(get_report))
        .route("/moderation/reports/{id}/status", put(update_status))
        .route("/moderation/actions", post(apply_action))
        .route("/moderation/audit", get(list_audit))
        .route("/moderation/users/{user_id}", get(user_status))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_target_types_accepted() {
        for t in ["stream", "recording", "user"] {
            assert!(MM_REPORT_TARGETS.contains(&t));
        }
    }

    #[test]
    fn unknown_target_type_rejected() {
        assert!(!MM_REPORT_TARGETS.contains(&"channel"));
        assert!(!MM_REPORT_TARGETS.contains(&""));
    }
}
