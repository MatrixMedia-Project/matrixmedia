//! /mm/v1/register* handlers. mm-core proxies signups via Synapse's
//! admin shared-secret API; Synapse `enable_registration: false`.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use mm_core::error::{ErrorCode, MMError};
use serde::{Deserialize, Serialize};

use crate::{client_ip, error::ApiError, honeypot, reserved_names, state::SharedState};

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

    // We can't use Synapse's /_matrix/client/v3/register/available — it requires
    // `enable_registration: true`, which we deliberately keep off (mm-core fronts
    // the admin shared-secret path). Instead, probe the public profile API: 404
    // means the local user doesn't exist (available); 200 means they do (taken).
    let synapse_url = state.config.matrix.homeserver_url.trim_end_matches('/');
    let server_name = state.config.matrix.server_name.as_str();
    if server_name.is_empty() {
        return Err(ApiError(MMError::Homeserver(
            "MM_MATRIX_SERVER_NAME not configured".to_string(),
        )));
    }
    let mxid = format!("@{username}:{server_name}");
    let url = format!(
        "{}/_matrix/client/v3/profile/{}/displayname",
        synapse_url,
        urlencoding::encode(&mxid)
    );

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| ApiError(MMError::Homeserver(format!("availability check: {e}"))))?;

    match resp.status().as_u16() {
        404 => Ok(Json(AvailabilityResp { available: true, reason: None })),
        200 => Ok(Json(AvailabilityResp { available: false, reason: Some("taken") })),
        code => Err(ApiError(MMError::Homeserver(format!(
            "unexpected profile-API status {code} for availability check",
        )))),
    }
}

#[derive(Deserialize)]
pub struct RegisterReq {
    pub username: String,
    pub password: String,
    pub tos_version: String,
    #[serde(default)]
    pub website: String, // honeypot — always "" from real clients
}

#[derive(Serialize)]
pub struct RegisterResp {
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
    /// Matrix server NAME (e.g. "matrix.steegler.com") — for MXID display.
    pub home_server: String,
    /// Matrix homeserver URL (e.g. "https://matrix.steegler.com") — what the
    /// client SDK passes to ClientBuilder.serverNameOrHomeserverUrl(...) and
    /// Session.homeserverUrl. Distinct from the signup endpoint URL (which is
    /// mm-core); in prod they're often the same host behind one ingress, but
    /// in local dev mm-core and Synapse live on different ports.
    pub homeserver_url: String,
}

pub async fn register(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Result<Json<RegisterResp>, ApiError> {
    let ip = client_ip::extract_client_ip(&headers);

    // 1. Honeypot — silently fail with generic 422
    honeypot::check(&state, &req.website).map_err(ApiError)?;

    // 2. Local validation (mirrors register_available + adds password + ToS)
    let username = req.username.trim().to_lowercase();
    if username.len() < 3 || username.len() > 64 {
        state.metrics.signups_failed_total.with_label_values(&["invalid_length"]).inc();
        return Err(ApiError(MMError::api(ErrorCode::UsernameInvalid, "bad length")));
    }
    if !username.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '=' | '-')) {
        state.metrics.signups_failed_total.with_label_values(&["bad_charset"]).inc();
        return Err(ApiError(MMError::api(ErrorCode::UsernameInvalid, "bad charset")));
    }
    if reserved_names::is_reserved(&username) {
        state.metrics.signups_failed_total.with_label_values(&["reserved"]).inc();
        return Err(ApiError(MMError::api(ErrorCode::UsernameReserved, "reserved")));
    }
    if req.password.len() < 8 {
        state.metrics.signups_failed_total.with_label_values(&["password_too_short"]).inc();
        return Err(ApiError(MMError::api(ErrorCode::InvalidRequest, "password too short")));
    }
    if req.tos_version != state.config.matrix.signup_tos_current_version {
        state.metrics.signups_failed_total.with_label_values(&["tos_version_mismatch"]).inc();
        return Err(ApiError(MMError::api(ErrorCode::InvalidRequest, "tos version mismatch")));
    }

    // 3. Per-IP rate-limit
    if let Err(retry_after_ms) = state.signup_limiter.allow(&ip) {
        state.metrics.signups_failed_total.with_label_values(&["rate_limited"]).inc();
        return Err(ApiError(MMError::Api {
            code: ErrorCode::RateLimitedSignup,
            message: "Rate limit exceeded".to_string(),
            retry_after_ms: Some(retry_after_ms),
        }));
    }

    // 4. Provision via Synapse admin shared-secret API
    let synapse_resp = state.synapse_admin.register(&username, &req.password).await.map_err(
        |e| {
            // Synapse error path — identify cause from the verbatim body surfaced by
            // SynapseAdminClient.
            let msg = format!("{e}");
            let client_err = if msg.contains("M_USER_IN_USE") {
                state.metrics.signups_failed_total.with_label_values(&["taken"]).inc();
                MMError::api(ErrorCode::UsernameTaken, "username taken")
            } else if msg.contains("M_INVALID_USERNAME") {
                state.metrics.signups_failed_total.with_label_values(&["synapse_invalid"]).inc();
                MMError::api(ErrorCode::UsernameInvalid, "invalid username")
            } else {
                state.metrics.signups_failed_total.with_label_values(&["synapse_other"]).inc();
                e
            };
            ApiError(client_err)
        },
    )?;

    // 5. Audit-trail write (log-on-failure, do not abort signup)
    if let Err(e) = mm_db::signups::record_signup(
        &state.signup_pool,
        &username,
        &synapse_resp.user_id,
        &req.tos_version,
        &ip,
        &state.config.matrix.signup_ip_hash_pepper,
    )
    .await
    {
        tracing::error!(?e, "signup audit-write failed (continuing)");
    }

    state.metrics.signups_total.inc();

    Ok(Json(RegisterResp {
        user_id: synapse_resp.user_id,
        access_token: synapse_resp.access_token,
        device_id: synapse_resp.device_id,
        home_server: synapse_resp.home_server,
        homeserver_url: state
            .config
            .matrix
            .public_homeserver_url
            .as_deref()
            .unwrap_or(&state.config.matrix.homeserver_url)
            .trim_end_matches('/')
            .to_string(),
    }))
}
