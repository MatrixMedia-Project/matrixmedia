//! HTTP contract of `GET /_mm/admin/v1/broadcast-servers` over a real listener: the route
//! wiring decides demo vs admin by role (no database needed — the route reads only the
//! snapshot cell).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use reqwest::StatusCode;
use serde_json::Value;

use mm_api::admin_broadcast_servers;
use mm_api::broadcast_servers::{
    Observations, OpenRecording, SnapshotCell, StreamObservation, SwitchObservation, Trackers,
    build_view,
};
use mm_api::middleware::AuthConfig;
use mm_api::stream_lifecycle::RoomLookup;
use mm_core::switch_client::{
    SwitchHealth, SwitchSource, SwitchViewer, switch_source_id, switch_viewer_id,
};
use mm_db::models::Stream;

const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";

const STREAM_ID: &str = "5b1e7c2d-0000-4000-8000-0000000000aa";
const TITLE: &str = "Private rehearsal title";
const HOST: &str = "@private-host:leak.example";
const VIEWER: &str = "@private-viewer:leak.example";

fn stream() -> Stream {
    Stream {
        id: STREAM_ID.to_string(),
        room_id: 1,
        host_user_id: HOST.into(),
        media_type: "video".into(),
        title: Some(TITLE.into()),
        status: "active".into(),
        sfu_room_id: Some(format!("mm-{STREAM_ID}")),
        participant_count: 0,
        started_at: Utc::now(),
        ended_at: None,
        state_event_id: None,
        feed_started_event_id: None,
        e2ee_enabled: false,
        e2ee_algorithm: None,
        e2ee_key_id: None,
        e2ee_key_generation: None,
        min_tier_level: None,
        ended_event_id: None,
        marker_generation: 1,
    }
}

/// A cell holding a real collected snapshot: values a demo caller must never see.
fn populated() -> Arc<SnapshotCell> {
    let obs = Observations {
        at: Utc::now(),
        switch: SwitchObservation::Reachable {
            health: SwitchHealth {
                sources: 1,
                viewers: 1,
                recorders: BTreeMap::from([("recording".to_string(), 1)]),
            },
            latency_ms: 3,
            sources: Ok(vec![SwitchSource {
                id: switch_source_id(STREAM_ID),
                source_type: "webrtc".into(),
                active: true,
            }]),
            viewers: Ok(vec![SwitchViewer {
                id: switch_viewer_id(STREAM_ID, VIEWER),
                current_source: switch_source_id(STREAM_ID),
                connected: true,
            }]),
        },
        livekit: Err("LiveKit health check timed out after 5 s".into()),
        streams: Ok(vec![StreamObservation {
            stream: stream(),
            room: RoomLookup::Failed,
            recordings: Ok(vec![OpenRecording {
                egress_id: Some(format!("mm-switch:{}", switch_source_id(STREAM_ID))),
                status: "recording".into(),
            }]),
        }]),
        sweep_grace_secs: 600,
        capacity_estimate: 50,
        turn_urls: 1,
    };
    let cell = Arc::new(SnapshotCell::new());
    cell.set(build_view(&obs, &mut Trackers::default()));
    cell
}

async fn start(cell: Arc<SnapshotCell>) -> String {
    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    let router = axum::Router::new()
        .nest("/_mm/admin/v1", admin_broadcast_servers::routes(cell))
        .layer(axum::Extension(auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/_mm/admin/v1/broadcast-servers")
}

fn admin_jwt(role: &str) -> String {
    mm_core::auth::issue_admin_session_token("@op:example.org", role, JWT_KEY).unwrap()
}

async fn get(url: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut req = reqwest::Client::new().get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.unwrap();
    (r.status(), r.text().await.unwrap())
}

#[tokio::test]
async fn demo_gets_structure_only_whatever_the_snapshot_holds() {
    let url = start(populated()).await;
    let (status, text) = get(&url, Some(&admin_jwt("demo"))).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    for leaked in [
        STREAM_ID,
        TITLE,
        HOST,
        "leak.example",
        "timed out",
        "\"ok\"",
        "\"recording\"",
    ] {
        assert!(
            !text.contains(leaked),
            "demo response leaked {leaked}: {text}"
        );
    }
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["demo"], true);
    assert!(body["collected_at"].is_null());
    assert!(body["capacity"].is_null());
    assert_eq!(body["broadcasts"], serde_json::json!([]));
    let servers = body["servers"].as_array().unwrap();
    let kinds: Vec<&str> = servers
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["mm-switch", "livekit", "livekit-egress", "coturn"]);
    for s in servers {
        assert!(s["role"].is_string(), "{s}");
        for field in [
            "status",
            "detail",
            "last_ok_at",
            "consecutive_failures",
            "latency_ms",
            "last_error",
        ] {
            assert!(s[field].is_null(), "demo {field} must be null: {s}");
        }
    }
}

#[tokio::test]
async fn the_static_admin_token_gets_the_snapshot_without_viewer_ids() {
    let url = start(populated()).await;
    let (status, text) = get(&url, Some(ADMIN_TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["demo"], false);
    assert!(body["collected_at"].is_string());
    let b = &body["broadcasts"][0];
    assert_eq!(b["stream_id"], STREAM_ID);
    assert_eq!(b["title"], TITLE);
    assert_eq!(b["host"], HOST);
    assert_eq!(b["switch_viewers"], 1);
    assert_eq!(b["recording"]["path"], "switch");
    assert_eq!(body["servers"][0]["status"], "ok");
    assert_eq!(
        body["servers"][1]["last_error"],
        "LiveKit health check timed out after 5 s"
    );
    assert_eq!(body["capacity"]["estimate"], 50);
    // The viewer is counted, never named.
    assert!(
        !text.contains("viewer-") && !text.contains("private-viewer"),
        "{text}"
    );
}

#[tokio::test]
async fn an_admin_session_gets_the_snapshot() {
    let url = start(populated()).await;
    let (status, text) = get(&url, Some(&admin_jwt("admin"))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains(TITLE), "{text}");
}

#[tokio::test]
async fn no_or_a_wrong_token_gets_no_snapshot() {
    let url = start(populated()).await;

    let (status, text) = get(&url, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    assert!(!text.contains(TITLE), "{text}");

    let (status, text) = get(&url, Some("not-the-admin-token-0123456789abcdef01234")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    assert!(!text.contains(TITLE), "{text}");
}
