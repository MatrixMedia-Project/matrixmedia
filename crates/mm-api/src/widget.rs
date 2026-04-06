use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use mm_core::auth::issue_session_token;
use mm_core::error::{ErrorCode, MMError};

use crate::error::ApiError;
use crate::state::SharedState;

/// Build widget backend routes.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/session", post(create_session))
        .route("/stream", get(get_stream_info))
        .route("/stream/join", post(join_stream))
        .route("/stream/leave", post(leave_stream))
        .with_state(state)
}

/// Request body for widget session creation.
#[derive(Debug, Deserialize)]
struct WidgetSessionRequest {
    /// OpenID access token from the Matrix client SDK.
    pub openid_access_token: String,
}

/// Response for widget session creation.
#[derive(Debug, Serialize)]
struct WidgetSessionResponse {
    pub mm_token: String,
    pub user_id: String,
    pub expires_in: u64,
}

/// POST /session -- Create a widget session (validates OpenID token).
///
/// 1. Accepts an OpenID token from the widget.
/// 2. Validates it against the homeserver.
/// 3. Returns an MM session JWT.
async fn create_session(
    State(state): State<SharedState>,
    Json(body): Json<WidgetSessionRequest>,
) -> Result<Json<WidgetSessionResponse>, ApiError> {
    // Validate the OpenID token against the homeserver.
    let hs = state.hs_client.clone();
    let user_id = state
        .token_cache
        .get_or_validate(&body.openid_access_token, |tok| async move {
            let info = hs.validate_openid(&tok).await.map_err(|e| {
                MMError::api(
                    ErrorCode::Forbidden,
                    format!("OpenID validation failed: {e}"),
                )
            })?;
            Ok(info.sub)
        })
        .await?;

    // Issue an MM session JWT (widget sessions use the same JWT format).
    let (mm_token, _refresh) = issue_session_token(&user_id, &state.config.jwt_signing_key)?;

    Ok(Json(WidgetSessionResponse {
        mm_token,
        user_id,
        expires_in: 900,
    }))
}

/// GET /stream -- Get current stream info for the widget.
///
/// Requires a `room_id` query parameter to look up the active stream.
async fn get_stream_info(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let room_id_str = params
        .get("room_id")
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "missing room_id query parameter"))?;

    let room_id = mm_core::types::RoomId(room_id_str.clone());
    let room = state
        .db
        .get_room_by_matrix_id(&room_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "room not found"))?;

    let stream = state.db.get_active_stream(room.id).await?;

    match stream {
        Some(s) => Ok(Json(serde_json::json!({
            "active": true,
            "stream_id": s.id,
            "host_user_id": s.host_user_id,
            "media_type": s.media_type,
            "title": s.title,
            "participant_count": s.participant_count,
            "started_at": s.started_at.to_rfc3339(),
        }))),
        None => Ok(Json(serde_json::json!({
            "active": false,
        }))),
    }
}

/// POST /stream/join -- Join stream from widget.
///
/// Delegates to the same logic as the client join_stream handler.
async fn join_stream(State(_state): State<SharedState>) -> Result<Json<Value>, ApiError> {
    // Widget join requires a session (validated via middleware in production).
    // For v1, widget join is handled by the client API; this is a placeholder.
    Err(MMError::api(
        ErrorCode::FeatureDisabled,
        "widget join not yet implemented; use the client API",
    )
    .into())
}

/// POST /stream/leave -- Leave stream from widget.
async fn leave_stream(State(_state): State<SharedState>) -> Result<Json<Value>, ApiError> {
    Err(MMError::api(
        ErrorCode::FeatureDisabled,
        "widget leave not yet implemented; use the client API",
    )
    .into())
}
