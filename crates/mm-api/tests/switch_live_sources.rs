//! `switch_live_sources` — the sweep's per-tick list of the broadcasts mm-switch is carrying —
//! against a stub mm-switch (axum on 127.0.0.1:0).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;

use mm_api::stream_lifecycle::{
    SWITCH_LIST_TIMEOUT_SECS, switch_live_sources, switch_live_sources_within,
};
use mm_core::switch_client::{SwitchClient, switch_source_id};

/// A limit generous enough for a loopback answer, short enough to keep the hang test quick.
const SHORT: Duration = Duration::from_millis(300);

async fn stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn only_the_active_sources_are_live() {
    let url = stub(Router::new().route(
        "/api/sources",
        get(|| async {
            axum::Json(json!({"sources": [
                {"id": "stream-a", "type": "webrtc", "active": true},
                {"id": "stream-b", "type": "webrtc", "active": false},
                {"id": "stream-c", "type": "webrtc", "active": true},
            ]}))
        }),
    ))
    .await;
    let client = SwitchClient::new(&url);

    let live = switch_live_sources_within(Some(&client), Duration::from_secs(5))
        .await
        .expect("the switch answered");
    assert_eq!(
        live,
        HashSet::from(["stream-a".to_string(), "stream-c".to_string()])
    );
    assert!(!live.contains("stream-b"), "an inactive source is not live");
}

#[tokio::test]
async fn a_stream_is_looked_up_by_its_switch_source_id() {
    let id = "3f2a1c9e-0000-4000-8000-00000000000a";
    let body = json!({"sources": [{"id": switch_source_id(id), "type": "webrtc", "active": true}]});
    let url = stub(Router::new().route(
        "/api/sources",
        get(move || {
            let body = body.clone();
            async move { axum::Json(body) }
        }),
    ))
    .await;
    let live = switch_live_sources_within(Some(&SwitchClient::new(&url)), Duration::from_secs(5))
        .await
        .expect("the switch answered");
    assert!(live.contains(&switch_source_id(id)));
    assert!(!live.contains(&switch_source_id("another-stream")));
}

#[tokio::test]
async fn a_switch_with_no_sources_is_an_empty_set_not_unknown() {
    // Answered, nothing live: the sweep must judge by LiveKit and the (empty) switch list,
    // which is `Some(empty)`, not `None`.
    let url = stub(Router::new().route(
        "/api/sources",
        get(|| async { axum::Json(json!({"sources": null})) }),
    ))
    .await;
    let live =
        switch_live_sources_within(Some(&SwitchClient::new(&url)), Duration::from_secs(5)).await;
    assert_eq!(live, Some(HashSet::new()));
}

#[tokio::test]
async fn a_401_is_unknown_not_an_empty_set() {
    let url = stub(Router::new().route(
        "/api/sources",
        get(|| async {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error": "unauthorized"})),
            )
        }),
    ))
    .await;
    let live =
        switch_live_sources_within(Some(&SwitchClient::new(&url)), Duration::from_secs(5)).await;
    assert_eq!(live, None);
}

#[tokio::test]
async fn a_hanging_switch_is_unknown_within_the_limit() {
    let url = stub(Router::new().route(
        "/api/sources",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            axum::Json(json!({"sources": []}))
        }),
    ))
    .await;
    let started = Instant::now();
    let live = switch_live_sources_within(Some(&SwitchClient::new(&url)), SHORT).await;
    assert_eq!(live, None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "gave up after the limit, not after the client's own timeout: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn nothing_listening_is_unknown() {
    let client = SwitchClient::new("http://127.0.0.1:1");
    assert_eq!(
        switch_live_sources_within(Some(&client), Duration::from_secs(5)).await,
        None
    );
}

#[tokio::test]
async fn no_switch_client_is_unknown_without_any_io() {
    // No client to ask: nothing to wait for, whatever the limit.
    let started = Instant::now();
    assert_eq!(switch_live_sources(None).await, None);
    assert_eq!(switch_live_sources_within(None, SHORT).await, None);
    assert!(started.elapsed() < SHORT);
}

#[test]
fn the_default_limit_is_five_seconds() {
    assert_eq!(SWITCH_LIST_TIMEOUT_SECS, 5);
}
