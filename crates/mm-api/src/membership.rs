//! Which Matrix rooms a user has joined — the membership source for listings
//! that must only show a caller the rooms they are in.
//!
//! mm-core keeps no membership table of its own: the appservice only reacts to
//! bot invites, and the feed indexer's member cache is keyed by room, not by
//! user. Synapse already holds the answer, so this asks its admin API:
//! `GET /_synapse/admin/v1/users/{user_id}/joined_rooms`, with the
//! `matrix.synapse_admin_token` the moderation surface uses. Synapse answers it
//! from a cached store lookup. It returns joined rooms only (an invite is not a
//! join), and for a remote user it returns the rooms our server shares with
//! them — which covers every MM room, since the bot that runs them is local.

use std::sync::atomic::{AtomicBool, Ordering};

use mm_core::error::MMError;
use mm_core::http::{DEP_SYNAPSE, SendTimed};
use serde::Deserialize;

#[derive(Deserialize)]
struct JoinedRoomsResp {
    joined_rooms: Vec<String>,
}

/// The Matrix room ids `user_id` has joined, per Synapse's admin API.
pub async fn synapse_joined_rooms(
    http: &reqwest::Client,
    homeserver_url: &str,
    admin_token: &str,
    user_id: &str,
) -> Result<Vec<String>, MMError> {
    let url = format!(
        "{}/_synapse/admin/v1/users/{}/joined_rooms",
        homeserver_url.trim_end_matches('/'),
        urlencoding::encode(user_id)
    );
    let resp = http
        .get(&url)
        .bearer_auth(admin_token)
        .send_timed(DEP_SYNAPSE)
        .await
        .map_err(|e| MMError::Homeserver(format!("joined_rooms request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(MMError::Homeserver(format!(
            "joined_rooms returned {status}: {body}"
        )));
    }
    let parsed: JoinedRoomsResp = resp
        .json()
        .await
        .map_err(|e| MMError::Homeserver(format!("joined_rooms parse failed: {e}")))?;
    Ok(parsed.joined_rooms)
}

/// The rooms `user_id` has joined, or none when that cannot be found out.
///
/// Fails closed: with no admin token configured, or when Synapse errors, the
/// answer is "no rooms", so a per-caller listing shrinks to the rows the caller
/// owns instead of showing rooms they may not be in.
pub async fn joined_rooms_or_none(
    http: &reqwest::Client,
    homeserver_url: &str,
    admin_token: &str,
    user_id: &str,
) -> Vec<String> {
    if admin_token.is_empty() {
        // Every client polls this, so say it once rather than per request.
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "MM_SYNAPSE_ADMIN_TOKEN not configured; room membership cannot be resolved, \
                 so callers see only the streams they host"
            );
        }
        return Vec::new();
    }
    match synapse_joined_rooms(http, homeserver_url, admin_token, user_id).await {
        Ok(rooms) => rooms,
        Err(e) => {
            tracing::warn!(user_id, error = %e, "joined-rooms lookup failed; showing only the caller's own streams");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use serde_json::json;

    /// What the stub saw, and how it should answer.
    #[derive(Default)]
    struct Stub {
        /// `(user_id path segment as decoded by axum, Authorization header)`.
        seen: Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    async fn joined_rooms(
        State(stub): State<Arc<Stub>>,
        Path(user_id): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        stub.seen.lock().unwrap().push((user_id, auth));
        if stub.fail {
            return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        axum::Json(json!({
            "joined_rooms": ["!a:hs", "!b:hs"],
            "total": 2,
        }))
        .into_response()
    }

    async fn serve(stub: Arc<Stub>) -> String {
        let app = Router::new()
            .route("/_synapse/admin/v1/users/{user_id}/joined_rooms", get(joined_rooms))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn asks_synapse_for_the_callers_joined_rooms_with_the_admin_token() {
        let stub = Arc::new(Stub::default());
        let base = serve(stub.clone()).await;

        let rooms = synapse_joined_rooms(
            &reqwest::Client::new(),
            &format!("{base}/"),
            "admin-tok",
            "@alice:hs",
        )
        .await
        .expect("joined_rooms should parse");

        assert_eq!(rooms, vec!["!a:hs".to_string(), "!b:hs".to_string()]);
        let seen = stub.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![("@alice:hs".to_string(), "Bearer admin-tok".to_string())],
            "one call, for the caller, with the admin token"
        );
    }

    #[tokio::test]
    async fn a_synapse_error_yields_no_rooms() {
        let stub = Arc::new(Stub {
            fail: true,
            ..Default::default()
        });
        let base = serve(stub.clone()).await;
        let http = reqwest::Client::new();

        assert!(
            synapse_joined_rooms(&http, &base, "admin-tok", "@alice:hs")
                .await
                .is_err()
        );
        assert!(
            joined_rooms_or_none(&http, &base, "admin-tok", "@alice:hs")
                .await
                .is_empty(),
            "a failed lookup must not widen what the caller sees"
        );
    }

    #[tokio::test]
    async fn no_admin_token_yields_no_rooms_without_calling_synapse() {
        let stub = Arc::new(Stub::default());
        let base = serve(stub.clone()).await;

        let rooms = joined_rooms_or_none(&reqwest::Client::new(), &base, "", "@alice:hs").await;

        assert!(rooms.is_empty());
        assert!(stub.seen.lock().unwrap().is_empty(), "no token, no request");
    }
}
