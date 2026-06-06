//! Content moderation (E3): report intake, operator queue, takedown actions,
//! append-only audit. New tables via `mm_db::moderation_db` (raw PgPool on
//! `state.signup_pool`); Synapse admin calls reuse the Bearer-token pattern.

use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::middleware::AuthUser;
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
