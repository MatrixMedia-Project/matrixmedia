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
use std::sync::{Arc, OnceLock};

use axum::routing::get;
use axum::{Json, Router};
use mm_core::fleet::{FleetNode, NodeFlavor, NodeId, NodeState, Ownership};
use mm_core::switch_client::{SwitchClient, switch_source_id, switch_viewer_id};
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};
use mm_fleet::runner::BroadcastCensus;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

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
        funding: mm_core::fleet::funding::Funding::Wallet,
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

/// A relay node pulls a stream from the switch it feeds off through a cascade session
/// (`cascade-{source id}-{relay node id}`), which that switch lists exactly like a viewer.
/// It is a connection between relays, not a person: counting it would grow the fleet and
/// fill nodes on demand that does not exist. Here a relay sits on node `relay-a` with one
/// real viewer, and a second relay pulls the same stream from it.
#[tokio::test]
async fn a_relay_pulling_from_a_node_is_not_counted_among_that_nodes_viewers() {
    let origin = fake_switch(
        json!([{ "id": "stream-b1", "type": "webrtc", "active": true }]),
        json!(null),
    )
    .await;
    let relay = fake_switch(
        json!(null),
        json!([
            { "id": "viewer-b1--alice-example.org", "current_source": "stream-b1", "connected": true },
            { "id": "cascade-stream-b1-lease-1-0",   "current_source": "stream-b1", "connected": true }
        ]),
    )
    .await;

    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin),
    )));
    pool.upsert(&node("relay-a"), Arc::new(SwitchClient::new(&relay)))
        .await;
    // viewers_per_node consults the switches only.
    let census = mm_api::fleet_census::SwitchCensus::new(pool, dummy_pool());

    let per_node = census.viewers_per_node().await;
    assert_eq!(
        per_node.get(&NodeId::new("relay-a")).copied(),
        Some(1),
        "alice is the node's one viewer; the relay pulling from it is not a second"
    );
}

/// The same fact for the per-stream count the planner grows the fleet on: the origin
/// carries one real viewer of the stream and one relay pulling it, so the live
/// broadcast has one viewer, not two. Needs a real `mm_streams` row, because the live
/// list is read from the database.
#[tokio::test]
async fn a_relay_pulling_a_stream_is_not_counted_as_a_viewer() {
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&db).await;
    let b1 = seed_active_stream(&db).await;

    let origin = fake_switch(
        json!([{ "id": switch_source_id(&b1), "type": "webrtc", "active": true }]),
        json!([
            { "id": switch_viewer_id(&b1, "@alice:example.org"), "current_source": switch_source_id(&b1), "connected": true },
            { "id": format!("cascade-{}-lease-1-0", switch_source_id(&b1)), "current_source": switch_source_id(&b1), "connected": true }
        ]),
    )
    .await;
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin),
    )));
    let census = mm_api::fleet_census::SwitchCensus::new(pool, db.clone());

    let live = census.live_broadcasts().await.expect("census");
    let viewers = live
        .iter()
        .find(|b| b.broadcast_id == b1)
        .map(|b| b.viewers);
    assert_eq!(
        viewers,
        Some(1),
        "alice watches; the relay pulling the stream is not a viewer"
    );

    // pool_sync's tick takes the same count through the combined gather.
    let (_, live) = census
        .viewers_per_node_and_live_broadcasts()
        .await
        .expect("census");
    let viewers = live
        .iter()
        .find(|b| b.broadcast_id == b1)
        .map(|b| b.viewers);
    assert_eq!(
        viewers,
        Some(1),
        "the combined gather must count the same way"
    );

    end_stream(&db, &b1).await;
}

async fn ensure_migrations(pool: &sqlx::PgPool) {
    static MIGRATIONS: OnceLock<Mutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| Mutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}

/// An active broadcast of its own, so other tests' streams never change its count.
async fn seed_active_stream(pool: &sqlx::PgPool) -> String {
    let db = PgDatabase::from_pool(pool.clone());
    let room = db
        .get_or_create_room(&RoomId(format!(
            "!census-{}:localhost",
            uuid::Uuid::new_v4()
        )))
        .await
        .expect("create room");
    db.create_stream(
        room.id,
        &UserId("@host:localhost".to_string()),
        Some("Census fixture"),
        "video",
        None,
        None,
    )
    .await
    .expect("create stream")
    .id
}

async fn end_stream(pool: &sqlx::PgPool, stream_id: &str) {
    PgDatabase::from_pool(pool.clone())
        .update_stream_status(&StreamId(stream_id.to_string()), StreamStatus::Ended)
        .await
        .expect("end stream");
}

/// `SwitchCensus::new` wants a PgPool. The tests that use this never reach the
/// database — `programme_is_live` and `viewers_per_node` consult the switches only —
/// so a lazily-connected pool at an unused port is honest: if a test ever DID query,
/// it would fail loudly rather than silently pass against a fake.
fn dummy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("lazy pool")
}
