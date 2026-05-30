//! Public announcement endpoint: `GET /mm/v1/announcements/active`.
//!
//! Returns the single highest-severity active announcement (or `{}` when
//! none). Backed by a 30s moka L1 cache on `AppState.announcement_cache`
//! plus `ETag` / `If-None-Match` for cheap 304 round-trips in steady state.
//!
//! Admin POST/DELETE handlers invalidate the cache so new banners propagate
//! within the next 60s client poll cycle (not 30s cache + 60s poll).

use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::state::SharedState;

/// Build the public announcements router (mounted under `/mm/v1`).
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/announcements/active", get(get_active_announcement))
        .with_state(state)
}

/// `GET /mm/v1/announcements/active` — return the active banner or `{}`.
///
/// Unauthenticated. Returns 304 when the request includes a matching
/// `If-None-Match` header. The ETag is `"<id>"` (the announcement row id);
/// for the empty-table case we deliberately omit the ETag so clients can't
/// cache "no banner" indefinitely against a stale match.
pub async fn get_active_announcement(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Cache lookup: returns a clone of the Option<AnnouncementRow>. We swallow
    // DB errors into `None` (an outage of the announcements feature should
    // never break clients) — the row is purely advisory.
    let pool = state.signup_pool.clone();
    let cached = state
        .announcement_cache
        .get_with((), async move {
            mm_db::announcements::get_active(&pool)
                .await
                .ok()
                .flatten()
        })
        .await;

    let Some(row) = cached else {
        // No active row: 200 + empty object. No ETag.
        return (StatusCode::OK, Json(json!({}))).into_response();
    };

    let etag_value = format!("\"{}\"", row.id);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if if_none_match == Some(etag_value.as_str()) {
        // 304 Not Modified — body is empty, but we still echo the ETag.
        let mut hdr = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&etag_value) {
            hdr.insert(header::ETAG, v);
        }
        return (StatusCode::NOT_MODIFIED, hdr).into_response();
    }

    // 200 + JSON body + ETag.
    let body: Value = serde_json::to_value(&row).unwrap_or_else(|_| json!({}));
    let mut hdr = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&etag_value) {
        hdr.insert(header::ETAG, v);
    }
    (StatusCode::OK, hdr, Json(body)).into_response()
}
