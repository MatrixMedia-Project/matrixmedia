//! The census against a stand-in mm-switch that serves the REAL response shapes.
//!
//! Why this needs a server rather than a unit test: `SwitchClient::list_sources`
//! ends in `serde_json::from_value(...).unwrap_or_default()`, so a shape
//! disagreement with the Go service does not error — it yields an **empty list**.
//! `programme_is_live` would then answer `false` for every live broadcast and the
//! fleet would silently never grow. The shapes below are copied from
//! `services/mm-switch/switch.go`'s `SourceInfo` and `ViewerInfo` JSON tags.
//!
//! One of them is a quirk worth pinning: Go's `var result []T` serialises an empty
//! collection as **`null`**, not `[]` — the live server returns
//! `{"viewers":null}` when nobody is watching, which this test reproduces.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState, Ownership};
use mm_core::switch_client::SwitchClient;
use mm_fleet::runner::BroadcastCensus;
use serde_json::json;
use tokio::net::TcpListener;

/// Starts a stand-in switch and returns its base URL.
async fn fake_switch(sources: serde_json::Value, viewers: serde_json::Value) -> String {
    let app = Router::new()
        .route(
            "/api/sources",
            get(move || {
                let s = sources.clone();
                async move { Json(json!({ "sources": s })) }
            }),
        )
        .route(
            "/api/viewers",
            get(move || {
                let v = viewers.clone();
                async move { Json(json!({ "viewers": v })) }
            }),
        );

    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

fn node(id: &str) -> FleetNode {
    FleetNode {
        id: NodeId::new(id),
        flavor: NodeFlavor::Fanout,
        ownership: Ownership::Rented,
        state: NodeState::Healthy,
        viewer_capacity: 250,
        viewers_current: 0,
    }
}

#[tokio::test]
async fn programme_is_live_when_the_origin_holds_an_active_stream_source() {
    let url = fake_switch(
        json!([
            { "id": "stream-b1", "type": "webrtc", "active": true },
            { "id": "ad-user-123", "type": "file", "active": true }
        ]),
        json!(null),
    )
    .await;

    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&url),
    )));
    // No database is touched by programme_is_live; a pool is enough.
    let census = mm_api::fleet_census::SwitchCensus::new(pool, dummy_pool());

    assert!(
        census.programme_is_live("b1").await.expect("query"),
        "an active source named stream-b1 means the programme is live"
    );
    assert!(
        !census.programme_is_live("b2").await.expect("query"),
        "a broadcast with no source of its own is not live"
    );
}

#[tokio::test]
async fn an_inactive_source_is_not_a_live_programme() {
    let url = fake_switch(
        json!([{ "id": "stream-b1", "type": "webrtc", "active": false }]),
        json!(null),
    )
    .await;
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&url),
    )));
    let census = mm_api::fleet_census::SwitchCensus::new(pool, dummy_pool());

    assert!(
        !census.programme_is_live("b1").await.expect("query"),
        "a registered but inactive source is a publisher that has gone away"
    );
}

/// The live server really does answer `{"sources":null}` — Go's `var result []T`
/// with nothing appended. If that were read as an error rather than as "no
/// sources", the census would fail on every idle switch.
#[tokio::test]
async fn a_null_source_list_reads_as_empty_not_as_a_failure() {
    let url = fake_switch(json!(null), json!(null)).await;
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&url),
    )));
    let census = mm_api::fleet_census::SwitchCensus::new(pool, dummy_pool());

    assert!(!census.programme_is_live("b1").await.expect("must not error"));
}

/// The shape pin. If mm-switch's JSON tags and `SwitchSource` ever disagree,
/// `list_sources` silently yields an empty vec and this fails — which is the only
/// signal, because nothing errors.
#[tokio::test]
async fn the_source_shape_from_the_go_service_still_parses() {
    // Copied verbatim from services/mm-switch/switch.go SourceInfo json tags.
    let url = fake_switch(
        json!([{ "id": "stream-shape", "type": "file", "active": true }]),
        json!(null),
    )
    .await;
    let client = SwitchClient::new(&url);
    let sources = client.list_sources().await.expect("list");
    assert_eq!(sources.len(), 1, "the Go response shape no longer parses");
    assert_eq!(sources[0].id, "stream-shape");
    assert_eq!(sources[0].source_type, "file");
    assert!(sources[0].active);
}

#[tokio::test]
async fn the_viewer_shape_from_the_go_service_still_parses() {
    let url = fake_switch(
        json!(null),
        json!([{ "id": "viewer-b1-@u-hs", "current_source": "stream-b1", "connected": true }]),
    )
    .await;
    let client = SwitchClient::new(&url);
    let viewers = client.list_viewers().await.expect("list");
    assert_eq!(viewers.len(), 1, "the Go response shape no longer parses");
    assert_eq!(viewers[0].current_source, "stream-b1");
    assert!(viewers[0].connected);
}

/// A node that will not answer is counted as zero rather than failing the census.
/// Under-counting makes the planner grow less; over-counting spends money, and
/// `plan()` never shrinks on a low count — so down is the safe direction.
#[tokio::test]
async fn a_node_that_does_not_answer_does_not_fail_the_census() {
    let origin = fake_switch(
        json!([{ "id": "stream-b1", "type": "webrtc", "active": true }]),
        json!([{ "id": "v1", "current_source": "stream-b1", "connected": true }]),
    )
    .await;

    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin),
    )));
    // A node pointing at a port nothing listens on.
    pool.upsert(
        &node("dead"),
        Arc::new(SwitchClient::new("http://127.0.0.1:1")),
    )
    .await;

    let census = mm_api::fleet_census::SwitchCensus::new(pool, dummy_pool());
    // programme_is_live only consults the origin, so it must still answer.
    assert!(census.programme_is_live("b1").await.expect("must not error"));
}

/// `SwitchCensus::new` wants a PgPool. These tests never reach the database —
/// `programme_is_live` consults the switch only — so a lazily-connected pool at an
/// unused port is honest: if a test ever DID query, it would fail loudly rather
/// than silently pass against a fake.
fn dummy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool")
}
