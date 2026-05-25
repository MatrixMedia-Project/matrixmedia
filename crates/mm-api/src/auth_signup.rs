//! /mm/v1/register* handlers. mm-core proxies signups via Synapse's
//! admin shared-secret API; Synapse `enable_registration: false`.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use mm_core::error::{ErrorCode, MMError};
use serde::{Deserialize, Serialize};

use crate::{client_ip, error::ApiError, reserved_names, state::SharedState};

pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/register/available", get(register_available))
        .route("/register", post(register))
        .with_state(state)
}

#[derive(Deserialize)]
pub struct AvailabilityQuery {
    pub username: String,
}

#[derive(Serialize)]
pub struct AvailabilityResp {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

pub async fn register_available(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(q): Query<AvailabilityQuery>,
) -> Result<Json<AvailabilityResp>, ApiError> {
    let ip = client_ip::extract_client_ip(&headers);
    if state.signup_avail_limiter.allow(&ip).is_err() {
        return Err(ApiError(MMError::api(
            ErrorCode::RateLimitedSignup,
            "too many availability checks",
        )));
    }

    let username = q.username.trim().to_lowercase();

    if username.is_empty() || username.len() < 3 {
        return Ok(Json(AvailabilityResp { available: false, reason: Some("too_short") }));
    }
    if username.len() > 64 {
        return Ok(Json(AvailabilityResp { available: false, reason: Some("too_long") }));
    }
    if !username.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '=' | '-')) {
        return Ok(Json(AvailabilityResp { available: false, reason: Some("invalid_charset") }));
    }
    if reserved_names::is_reserved(&username) {
        return Ok(Json(AvailabilityResp { available: false, reason: Some("reserved") }));
    }

    // Ask Synapse — homeserver URL comes from config
    let synapse_url = state.config.matrix.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{}/_matrix/client/v3/register/available?username={}",
        synapse_url,
        urlencoding::encode(&username)
    );

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| ApiError(MMError::Homeserver(format!("availability check: {e}"))))?;

    if resp.status().is_success() {
        Ok(Json(AvailabilityResp { available: true, reason: None }))
    } else {
        // Synapse returns 400 M_USER_IN_USE for taken
        Ok(Json(AvailabilityResp { available: false, reason: Some("taken") }))
    }
}

/// Placeholder — Task B2 will implement.
pub async fn register() -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "register handler — implemented in Task B2")
}
