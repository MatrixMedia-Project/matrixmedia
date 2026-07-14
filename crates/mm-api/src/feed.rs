//! Client-facing newsfeed endpoints.
//!
//! - `GET  /_mm/client/v1/feed`       — keyset-paginated feed read
//! - `POST /_mm/client/v1/feed/seen`  — idempotent "I've seen these items"
//!
//! Both endpoints are gated by the `MM_FEED_ENABLED` env var (dark-launch
//! gate per the Phase-1 plan §C-1). When `false`, both handlers return 404
//! before doing any work so an accidental enable on prod doesn't leak
//! schema details. Once an operator flips the env var to `true`, the
//! gate is a single environment check per request.
//!
//! Mute exclusion: the read fetches the user's
//! `com.steegler.matrixmedia.feed_muted` account_data list from Synapse
//! (30s in-process cache) and filters muted rooms at the SQL level via
//! `mm_db::feed_db::get_feed_items`.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use mm_core::error::{ErrorCode, MMError};
use serde::Deserialize;

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

/// Build the feed router (mounted under `/_mm/client/v1`).
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/feed", get(get_feed))
        .route("/feed/seen", post(post_feed_seen))
        .with_state(state)
}

/// Returns `true` when the newsfeed endpoints are enabled.
///
/// Defaults to `true` (so dev/test deploys don't have to set it) — operator
/// must explicitly set `MM_FEED_ENABLED=false` to dark-launch.
fn feed_enabled() -> bool {
    match std::env::var("MM_FEED_ENABLED") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "off"),
        Err(_) => true,
    }
}

#[derive(Debug, Deserialize)]
pub struct FeedQueryParams {
    /// Opaque keyset cursor (returned as `next` on the previous page).
    pub since: Option<String>,
    /// Page size — clamped to `[1, 100]`. Default 20.
    pub limit: Option<i64>,
    /// Comma-separated list of feed kinds: `broadcast.started`,
    /// `broadcast.ended`, `recording.available`, `post`.
    pub kinds: Option<String>,
    /// Restrict to a single room id.
    pub room_id: Option<String>,
}

/// `GET /_mm/client/v1/feed`
pub async fn get_feed(
    State(state): State<SharedState>,
    auth: AuthUser,
    Query(params): Query<FeedQueryParams>,
) -> Result<impl IntoResponse, ApiError> {
    if !feed_enabled() {
        return Ok((StatusCode::NOT_FOUND, Json(serde_json::json!({}))).into_response());
    }

    // Per-user rate limit. We re-use SignupRateLimiter (keyed on String);
    // the key is the user MXID so cross-IP abuse from the same account
    // still throttles correctly.
    if state.feed_limiter.allow(&auth.user_id.0).is_err() {
        return Err(ApiError(MMError::api(
            ErrorCode::RateLimited,
            "feed rate limit exceeded",
        )));
    }

    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let since = match params.since.as_deref() {
        Some(s) => Some(mm_db::feed_db::FeedCursor::decode(s).map_err(|msg| {
            ApiError(MMError::api(ErrorCode::InvalidRequest, msg))
        })?),
        None => None,
    };
    let kinds: Vec<String> = params
        .kinds
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let kinds_refs: Vec<&str> = kinds.iter().map(String::as_str).collect();

    let muted = fetch_muted_rooms(&state, &auth.user_id.0).await;

    let page = mm_db::feed_db::get_feed_items(
        &state.signup_pool,
        &auth.user_id.0,
        since,
        limit,
        &kinds_refs,
        params.room_id.as_deref(),
        &muted,
    )
    .await
    .map_err(|e| ApiError(MMError::Database(e.to_string())))?;

    Ok((StatusCode::OK, Json(page)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct FeedSeenRequest {
    /// Hex-encoded `mm_feed_items.id` values to mark seen. Max 100.
    pub ids: Vec<String>,
}

/// `POST /_mm/client/v1/feed/seen`
pub async fn post_feed_seen(
    State(state): State<SharedState>,
    auth: AuthUser,
    Json(body): Json<FeedSeenRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if !feed_enabled() {
        return Ok((StatusCode::NOT_FOUND, Json(serde_json::json!({}))).into_response());
    }

    if body.ids.is_empty() {
        return Err(ApiError(MMError::api(
            ErrorCode::InvalidRequest,
            "ids must not be empty",
        )));
    }
    if body.ids.len() > 100 {
        return Err(ApiError(MMError::api(
            ErrorCode::InvalidRequest,
            "ids capped at 100 per request",
        )));
    }

    let mut decoded: Vec<Vec<u8>> = Vec::with_capacity(body.ids.len());
    for id in &body.ids {
        let bytes = hex::decode(id).map_err(|_| {
            ApiError(MMError::api(
                ErrorCode::InvalidRequest,
                "ids must be hex-encoded",
            ))
        })?;
        decoded.push(bytes);
    }

    mm_db::feed_db::mark_seen(&state.signup_pool, &auth.user_id.0, &decoded)
        .await
        .map_err(|e| ApiError(MMError::Database(e.to_string())))?;

    Ok((StatusCode::OK, Json(serde_json::json!({}))).into_response())
}

/// Fetch the user's muted room list from Matrix account_data, with a 30s
/// in-process cache to avoid hammering Synapse on every page request.
///
/// Errors degrade to "no muted rooms" rather than failing the feed read.
async fn fetch_muted_rooms(state: &SharedState, user_id: &str) -> Vec<String> {
    if let Some(cached) = state.feed_cache.get(&user_id.to_string()).await {
        return cached;
    }
    let fetched = fetch_muted_rooms_uncached(state, user_id).await;
    state
        .feed_cache
        .insert(user_id.to_string(), fetched.clone())
        .await;
    fetched
}

async fn fetch_muted_rooms_uncached(state: &SharedState, user_id: &str) -> Vec<String> {
    // `GET /_matrix/client/v3/user/{user_id}/account_data/{type}` returns
    // either the stored JSON object (200) or 404 if the account_data
    // type was never set. Anything else degrades to "no muted rooms".
    let url = format!(
        "{}/_matrix/client/v3/user/{}/account_data/com.steegler.matrixmedia.feed_muted",
        state.config.matrix.homeserver_url,
        urlencoding::encode(user_id)
    );
    let client = mm_core::http::shared();
    let resp = match client
        .get(&url)
        .bearer_auth(&state.config.matrix.as_token)
        .query(&[("user_id", user_id)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "feed_muted account_data fetch failed");
            return Vec::new();
        }
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // Two shapes supported: `{"rooms": ["!a:hs", "!b:hs"]}` or a flat array.
    if let Some(arr) = body.get("rooms").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    if let Some(arr) = body.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_enabled_default_true() {
        // SAFETY: tests run single-threaded for env mutation.
        unsafe {
            std::env::remove_var("MM_FEED_ENABLED");
        }
        assert!(feed_enabled());
    }

    #[test]
    fn feed_enabled_respects_false() {
        unsafe {
            std::env::set_var("MM_FEED_ENABLED", "false");
        }
        assert!(!feed_enabled());
        unsafe {
            std::env::set_var("MM_FEED_ENABLED", "0");
        }
        assert!(!feed_enabled());
        unsafe {
            std::env::set_var("MM_FEED_ENABLED", "true");
        }
        assert!(feed_enabled());
        unsafe {
            std::env::remove_var("MM_FEED_ENABLED");
        }
    }
}
