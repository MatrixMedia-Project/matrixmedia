//! The relay tree against stand-in switches. The stand-ins serve the real response
//! shapes (see `tests/fleet_census.rs`), keep the cascade sources they are given the
//! way mm-switch does (a second seed of the same id answers 409), and record every
//! seed and every source removal they are sent.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mm_core::config::{Config, FleetMode};
use mm_core::config_handle::ConfigHandle;
use mm_core::fleet::NodeId;
use mm_core::switch_client::{SwitchClient, switch_source_id, switch_viewer_id};
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// What a stand-in relay was asked to do.
#[derive(Default)]
struct Calls {
    /// Bodies of every `POST /api/sources/cascade`, answered 201 or 409 alike.
    seeds: Vec<Value>,
    /// Ids of every `DELETE /api/sources/{id}`.
    removals: Vec<String>,
    /// Cascade sources it holds now.
    held: HashSet<String>,
    /// Answer every removal with a 500 and keep the source.
    fail_removals: bool,
    /// A feed row another server instance records for this relay while a seed is in
    /// flight: inserted just before the seed is answered. (database, stream, node,
    /// feeder, depth).
    written_elsewhere: Option<(sqlx::PgPool, String, String, Option<String>, i16)>,
    /// List every source it holds as inactive: no picture (its feeder is gone).
    dead: bool,
}

type Recorder = Arc<Mutex<Calls>>;

/// The tests here share one database, and a sync plans every healthy relay in it: two
/// tests running at once would plan each other's relays for their streams. One at a time.
static SERIAL: Mutex<()> = Mutex::const_new(());

/// A stand-in origin: fixed source and viewer lists. Returns its base URL.
async fn fake_origin(sources: Value, viewers: Value) -> String {
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
    serve(app).await
}

/// A stand-in relay listing `viewers`. Returns its base URL and what it was asked.
async fn fake_relay(viewers: Value) -> (String, Recorder) {
    let calls: Recorder = Arc::default();
    let app = Router::new()
        .route(
            "/api/sources",
            get(|State(calls): State<Recorder>| async move {
                let calls = calls.lock().await;
                let held: Vec<Value> = calls
                    .held
                    .iter()
                    .map(|id| json!({ "id": id, "type": "cascade", "active": !calls.dead }))
                    .collect();
                Json(json!({ "sources": held }))
            }),
        )
        .route(
            "/api/viewers",
            get(move || {
                let v = viewers.clone();
                async move { Json(json!({ "viewers": v })) }
            }),
        )
        .route(
            "/api/sources/cascade",
            post(|State(calls): State<Recorder>, Json(body): Json<Value>| async move {
                let id = body["id"].as_str().unwrap_or_default().to_string();
                let mut calls = calls.lock().await;
                calls.seeds.push(body);
                if let Some((db, stream, node, feeder, depth)) = calls.written_elsewhere.take() {
                    sqlx::query(
                        "INSERT INTO mm_fleet_relay_feeds (stream_id, node_id, feeder_node_id, depth)
                         VALUES ($1, $2, $3, $4)",
                    )
                    .bind(stream)
                    .bind(node)
                    .bind(feeder)
                    .bind(depth)
                    .execute(&db)
                    .await
                    .expect("insert the other instance's row");
                }
                if calls.held.insert(id.clone()) {
                    (StatusCode::CREATED, Json(json!({ "id": id, "status": "pending" })))
                } else {
                    (StatusCode::CONFLICT, Json(json!({ "error": "already exists" })))
                }
            }),
        )
        .route(
            "/api/sources/{id}/cascade-token",
            post(|| async { Json(json!({ "ok": "true" })) }),
        )
        .route(
            "/api/sources/{id}",
            delete(|State(calls): State<Recorder>, Path(id): Path<String>| async move {
                let mut calls = calls.lock().await;
                calls.removals.push(id.clone());
                if calls.fail_removals {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "boom" })));
                }
                calls.held.remove(&id);
                (StatusCode::OK, Json(json!({ "ok": "true" })))
            }),
        )
        .with_state(calls.clone());
    (serve(app).await, calls)
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

/// The pool-sync context of these tests: fleet on, a threshold of one viewer, and each
/// relay address mapped to its stand-in.
fn context(addresses: HashMap<String, String>) -> mm_api::pool_sync::PoolSyncContext {
    let mut config = Config::default();
    config.fleet.mode = FleetMode::On;
    config.fleet.relay_viewer_threshold = 1;
    mm_api::pool_sync::PoolSyncContext::new(
        ConfigHandle::new(config),
        Some("test-secret".to_string()),
        None,
    )
    .with_relay_base_url(move |ip| {
        addresses
            .get(ip)
            .cloned()
            // Nodes other tests left behind: refused at once, never one of ours.
            .unwrap_or_else(|| "http://127.0.0.1:1".to_string())
    })
    // Turn 0 on every pass: the entry slot goes to the lowest id, and these tests' relay
    // ids sort before anything other tests may have left in the database.
    .with_tick(Duration::from_secs(1 << 40))
}

/// The origins (`origin_url`) of the seeds `calls` received for `source`, in order.
async fn seeded_from(calls: &Recorder, source: &str) -> Vec<Value> {
    calls
        .lock()
        .await
        .seeds
        .iter()
        .filter(|b| b["id"] == json!(source))
        .map(|b| b["origin_url"].clone())
        .collect()
}

async fn removals_of(calls: &Recorder, source: &str) -> usize {
    calls
        .lock()
        .await
        .removals
        .iter()
        .filter(|id| *id == source)
        .count()
}

async fn feeds(db: &sqlx::PgPool, stream_id: &str) -> Vec<(String, Option<String>, i16)> {
    let mut rows: Vec<(String, Option<String>, i16)> = sqlx::query_as(
        "SELECT node_id, feeder_node_id, depth FROM mm_fleet_relay_feeds WHERE stream_id = $1",
    )
    .bind(stream_id)
    .fetch_all(db)
    .await
    .expect("read feeds");
    rows.sort();
    rows
}

/// One stream through the life of its relay tree: the first relay is fed by the entry,
/// and the second, a pass later, by the first; every row is seeded again on every pass; a feeder that
/// leaves the relay list removes nothing downstream and changes no row until the
/// downstream source is re-created from its fallback; a stream that ended has its
/// sources removed, and a failed removal keeps its row.
#[tokio::test]
async fn a_relay_is_seeded_with_its_feeders_address() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    // Ids that sort before any other node a previous test left behind: relays are
    // visited in id order, so these two are planned first.
    let (node_a, node_b) = (format!("0-feeds-{tag}-a"), format!("0-feeds-{tag}-b"));
    let provider = format!("feeds-{tag}");
    let zone = "ams-1";

    let b1 = seed_active_stream(&db).await;
    sqlx::query("UPDATE mm_streams SET media_path = 'relay_after_threshold' WHERE id = $1")
        .bind(&b1)
        .execute(&db)
        .await
        .expect("set media_path");
    let source = switch_source_id(&b1);

    // The entry: one person watching the stream.
    let origin_url = fake_origin(
        json!([{ "id": source, "type": "webrtc", "active": true }]),
        json!([{ "id": switch_viewer_id(&b1, "@alice:example.org"), "current_source": source, "connected": true }]),
    )
    .await;
    let (url_a, calls_a) = fake_relay(json!(null)).await;
    let (url_b, calls_b) = fake_relay(json!(null)).await;

    seed_provider_zone(&db, &provider, zone).await;
    seed_relay(&db, &node_a, "192.0.2.1", &provider, zone).await;
    seed_relay(&db, &node_b, "192.0.2.2", &provider, zone).await;

    let ctx = context(HashMap::from([
        ("192.0.2.1".to_string(), url_a.clone()),
        ("192.0.2.2".to_string(), url_b.clone()),
    ]));
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));

    // The relays seeded with the entry as their origin: the main server's copies.
    let from_entry = |to_a: &[Value], to_b: &[Value]| -> usize {
        usize::from(to_a.contains(&json!(origin_url))) + usize::from(to_b.contains(&json!(origin_url)))
    };

    // Pass 1: only a (the lowest id, at turn 0) joins, from the entry. b's only choice
    // would be the entry too, so it waits for a's row: a relay feeds others only once its
    // seed is confirmed.
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    let to_a = seeded_from(&calls_a, &source).await;
    let to_b = seeded_from(&calls_b, &source).await;
    assert_eq!(to_a, vec![json!(origin_url)], "the first relay pulls from the entry");
    assert!(to_b.is_empty(), "b waits a tick: {to_b:?}");
    assert_eq!(from_entry(&to_a, &to_b), 1, "the origin feeds exactly one relay");
    assert_eq!(feeds(&db, &b1).await, vec![(node_a.clone(), None, 1)]);
    // Placement: the census's people count is recorded, and a source just created has no
    // picture yet, so no relay is offered to a viewer.
    let stream = StreamId(b1.clone());
    let (id_a, id_b) = (NodeId::new(&node_a), NodeId::new(&node_b));
    assert_eq!(pool.viewers_of(&stream).await, Some(1));
    assert!(
        pool.carried_by(&id_a).await.is_empty(),
        "just created: not carried yet"
    );
    assert!(pool.assign_fanout_node(&stream).await.is_none());

    // Pass 2: b joins under the stored a; a is seeded again with the same origin (409).
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    let to_a = seeded_from(&calls_a, &source).await;
    let to_b = seeded_from(&calls_b, &source).await;
    assert_eq!(to_a, vec![json!(origin_url), json!(origin_url)], "a kept row is seeded again");
    assert_eq!(
        to_b,
        vec![json!(url_a)],
        "the second relay pulls from the first relay, not from the entry"
    );
    let seed_b = calls_b.lock().await.seeds[0].clone();
    assert_eq!(seed_b["relay_node_id"], json!(node_b));
    assert_eq!(from_entry(&to_a, &to_b), 1, "still exactly one relay pulls from the entry");
    assert_eq!(
        pool.carried_by(&id_a).await,
        HashSet::from([b1.clone()]),
        "a's source read active: a carries the stream"
    );
    assert!(
        pool.carried_by(&id_b).await.is_empty(),
        "b's was just created"
    );
    assert_eq!(
        feeds(&db, &b1).await,
        vec![
            (node_a.clone(), None, 1),
            (node_b.clone(), Some(node_a.clone()), 2),
        ]
    );

    // Pass 2b, nothing changed: both rows are seeded again with the same origin (the
    // relays answer 409 and keep their source), and nothing is removed.
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(seeded_from(&calls_a, &source).await.len(), 3);
    assert_eq!(
        seeded_from(&calls_b, &source).await,
        vec![json!(url_a), json!(url_a)]
    );
    assert_eq!(removals_of(&calls_a, &source).await, 0);
    assert_eq!(removals_of(&calls_b, &source).await, 0);
    assert_eq!(pool.carried_by(&id_b).await, HashSet::from([b1.clone()]));
    let (picked, _) = pool
        .assign_fanout_node(&stream)
        .await
        .expect("both carry b1");
    assert_eq!(picked, id_a, "equal headroom: the lowest id");

    // Pass 2c: a is rented and 6 minutes from its destroy deadline. It still carries the
    // stream, but takes no new viewers: b does.
    sqlx::query(
        "UPDATE mm_fleet_nodes SET ownership = 'rented', destroy_deadline = now() + interval '6 minutes'
          WHERE mm_node_id = $1",
    )
    .bind(&node_a)
    .execute(&db)
    .await
    .expect("rent a");
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(pool.carried_by(&id_a).await, HashSet::from([b1.clone()]));
    let (picked, _) = pool
        .assign_fanout_node(&stream)
        .await
        .expect("b carries b1");
    assert_eq!(picked, id_b, "a relay in its last minutes is not offered");

    // Pass 3: a starts draining, so it leaves the relay list while it may still serve b.
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'draining' WHERE mm_node_id = $1")
        .bind(&node_a)
        .execute(&db)
        .await
        .expect("drain a");
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(
        removals_of(&calls_b, &source).await,
        0,
        "b's source is never removed because its feeder left: people may be watching it"
    );
    assert_eq!(
        removals_of(&calls_a, &source).await,
        0,
        "a draining node is not called at all"
    );
    assert_eq!(
        seeded_from(&calls_b, &source).await.last(),
        Some(&json!(origin_url)),
        "b is ensured against its fallback, the entry"
    );
    assert!(
        pool.carried_by(&id_a).await.is_empty(),
        "evicted: a carries nothing"
    );
    assert_eq!(
        feeds(&db, &b1).await,
        vec![(node_b.clone(), Some(node_a.clone()), 2)],
        "b's source is alive (409), so its row still names where it pulls from: a"
    );

    // Pass 4: b's source is gone (the relay lost it). The next seed creates it from the
    // fallback, and only now does the row change.
    calls_b.lock().await.held.remove(&source);
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(
        seeded_from(&calls_b, &source).await.last(),
        Some(&json!(origin_url))
    );
    assert_eq!(
        feeds(&db, &b1).await,
        vec![(node_b.clone(), None, 1)],
        "the re-created source pulls from the entry, and the row says so"
    );

    // Pass 5: the stream ends, but b refuses the removal: the row stays.
    PgDatabase::from_pool(db.clone())
        .update_stream_status(&StreamId(b1.clone()), StreamStatus::Ended)
        .await
        .expect("end stream");
    calls_b.lock().await.fail_removals = true;
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(removals_of(&calls_b, &source).await, 1, "the removal was tried");
    assert_eq!(
        feeds(&db, &b1).await,
        vec![(node_b.clone(), None, 1)],
        "a failed removal keeps its row"
    );

    // Pass 6: b accepts. Nobody can be watching an ended stream: source and row go.
    calls_b.lock().await.fail_removals = false;
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(removals_of(&calls_b, &source).await, 2);
    assert!(feeds(&db, &b1).await.is_empty(), "and its row with it");

    cleanup(&db, &b1, &[&node_a, &node_b], &provider).await;
}

/// People on an ad break are on an `ad-…` source that names no programme. A relay with
/// such a session holds every live stream it carries, even one that is no longer wanted.
#[tokio::test]
async fn an_ad_break_holds_the_relay_it_is_on() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let node = format!("0-feeds-{tag}-ad");
    let provider = format!("feeds-{tag}");
    let zone = "ams-1";

    // State `core`: never wanted, so only `held` can keep the row.
    let b1 = seed_active_stream(&db).await;
    let source = switch_source_id(&b1);
    let origin_url = fake_origin(
        json!([{ "id": source, "type": "webrtc", "active": true }]),
        json!(null),
    )
    .await;
    // The relay's only connected session is a person on an ad break.
    let (url, calls) = fake_relay(json!([
        { "id": switch_viewer_id(&b1, "@bob:example.org"), "current_source": "ad-bob-1", "connected": true }
    ]))
    .await;

    seed_provider_zone(&db, &provider, zone).await;
    seed_relay(&db, &node, "192.0.2.3", &provider, zone).await;
    sqlx::query(
        "INSERT INTO mm_fleet_relay_feeds (stream_id, node_id, feeder_node_id, depth) VALUES ($1, $2, NULL, 1)",
    )
    .bind(&b1)
    .bind(&node)
    .execute(&db)
    .await
    .expect("insert feed");

    let ctx = context(HashMap::from([("192.0.2.3".to_string(), url)]));
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;

    assert_eq!(removals_of(&calls, &source).await, 0, "the ad break's relay keeps its source");
    assert_eq!(feeds(&db, &b1).await, vec![(node.clone(), None, 1)]);

    cleanup(&db, &b1, &[&node], &provider).await;
}

/// A relay that already holds a stream's source no row describes (a seed that was cut
/// off before its row was written) is adopted as a leaf of unknown origin: (None, 3). It
/// can then be removed like any row when the stream ends.
#[tokio::test]
async fn a_source_found_with_no_row_is_adopted_as_a_leaf() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let node = format!("0-feeds-{tag}-adopt");
    let provider = format!("feeds-{tag}");
    let zone = "ams-1";

    let b1 = seed_active_stream(&db).await;
    sqlx::query("UPDATE mm_streams SET media_path = 'relay_after_threshold' WHERE id = $1")
        .bind(&b1)
        .execute(&db)
        .await
        .expect("set media_path");
    let source = switch_source_id(&b1);
    let origin_url = fake_origin(
        json!([{ "id": source, "type": "webrtc", "active": true }]),
        json!([{ "id": switch_viewer_id(&b1, "@alice:example.org"), "current_source": source, "connected": true }]),
    )
    .await;
    let (url, calls) = fake_relay(json!(null)).await;
    calls.lock().await.held.insert(source.clone());

    seed_provider_zone(&db, &provider, zone).await;
    seed_relay(&db, &node, "192.0.2.4", &provider, zone).await;
    let ctx = context(HashMap::from([("192.0.2.4".to_string(), url)]));
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));

    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(seeded_from(&calls, &source).await, vec![json!(origin_url)]);
    assert_eq!(
        feeds(&db, &b1).await,
        vec![(node.clone(), None, 3)],
        "the relay answered 409: adopted as a leaf, origin unknown"
    );

    PgDatabase::from_pool(db.clone())
        .update_stream_status(&StreamId(b1.clone()), StreamStatus::Ended)
        .await
        .expect("end stream");
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(removals_of(&calls, &source).await, 1, "an adopted leaf is removed like any row");
    assert!(feeds(&db, &b1).await.is_empty());

    cleanup(&db, &b1, &[&node], &provider).await;
}

/// Two server instances: this one planned to add the relay (no row when it read them),
/// and while its seed is in flight the other instance's seed creates the source and
/// records where it pulls from. This one's seed then finds the source already there. Its
/// adoption must not overwrite the confirmed row: that row is what holds the feeder.
#[tokio::test]
async fn an_adoption_never_overwrites_a_row_written_meanwhile() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let node = format!("0-feeds-{tag}-raced");
    let feeder = format!("0-feeds-{tag}-feeder");
    let provider = format!("feeds-{tag}");
    let zone = "ams-1";

    let b1 = seed_active_stream(&db).await;
    sqlx::query("UPDATE mm_streams SET media_path = 'relay_after_threshold' WHERE id = $1")
        .bind(&b1)
        .execute(&db)
        .await
        .expect("set media_path");
    let source = switch_source_id(&b1);
    let origin_url = fake_origin(
        json!([{ "id": source, "type": "webrtc", "active": true }]),
        json!([{ "id": switch_viewer_id(&b1, "@alice:example.org"), "current_source": source, "connected": true }]),
    )
    .await;
    let (url, calls) = fake_relay(json!(null)).await;
    {
        let mut calls = calls.lock().await;
        // The other instance's seed got there first: the source exists...
        calls.held.insert(source.clone());
        // ...and its row is written while this instance's seed is in flight.
        calls.written_elsewhere = Some((
            db.clone(),
            b1.clone(),
            node.clone(),
            Some(feeder.clone()),
            2,
        ));
    }

    seed_provider_zone(&db, &provider, zone).await;
    seed_relay(&db, &node, "192.0.2.5", &provider, zone).await;
    let ctx = context(HashMap::from([("192.0.2.5".to_string(), url)]));
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));

    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    assert_eq!(seeded_from(&calls, &source).await, vec![json!(origin_url)]);
    assert_eq!(
        feeds(&db, &b1).await,
        vec![(node.clone(), Some(feeder.clone()), 2)],
        "the confirmed row is left exactly as it was"
    );

    cleanup(&db, &b1, &[&node], &provider).await;
}

/// A stream's only first-level relay dies and its three children are orphans whose sources
/// are dead. Re-creating every one of them from the entry would have the main server send
/// three copies for the rest of the stream; one is re-created from the entry, and the others
/// a tick later from that one. A row already recorded as fed by the entry is re-created from
/// the entry at once, whoever took the stream's entry this tick.
#[tokio::test]
async fn one_orphan_per_stream_and_tick_is_re_created_from_the_entry() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    // The first-level relay that died: it has a row, and no node any more.
    let dead_feeder = format!("0-feeds-{tag}-x");
    let orphans: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|s| format!("0-feeds-{tag}-{s}"))
        .collect();
    // An adopted leaf, recorded as fed by the entry, in another zone so that it never
    // feeds the orphans.
    let leaf = format!("0-feeds-{tag}-leaf");
    let (provider, provider_b) = (format!("feeds-{tag}"), format!("feeds-{tag}-b"));

    let b1 = seed_active_stream(&db).await;
    sqlx::query("UPDATE mm_streams SET media_path = 'relay_after_threshold' WHERE id = $1")
        .bind(&b1)
        .execute(&db)
        .await
        .expect("set media_path");
    let source = switch_source_id(&b1);
    let origin_url = fake_origin(
        json!([{ "id": source, "type": "webrtc", "active": true }]),
        json!([{ "id": switch_viewer_id(&b1, "@alice:example.org"), "current_source": source, "connected": true }]),
    )
    .await;

    seed_provider_zone(&db, &provider, "ams-1").await;
    seed_provider_zone(&db, &provider_b, "ams-2").await;
    let mut addresses = HashMap::new();
    let mut calls: HashMap<String, Recorder> = HashMap::new();
    for (i, node) in orphans.iter().chain([&leaf]).enumerate() {
        let ip = format!("192.0.2.{}", 10 + i);
        let (url, recorder) = fake_relay(json!(null)).await;
        {
            let mut r = recorder.lock().await;
            r.held.insert(source.clone());
            r.dead = true;
        }
        let (prov, zone) = if *node == leaf {
            (&provider_b, "ams-2")
        } else {
            (&provider, "ams-1")
        };
        seed_relay(&db, node, &ip, prov, zone).await;
        addresses.insert(ip, url);
        calls.insert(node.clone(), recorder);
    }
    for (node, feeder, depth) in orphans
        .iter()
        .map(|o| (o.as_str(), Some(dead_feeder.as_str()), 2_i16))
        .chain([(dead_feeder.as_str(), None, 1), (leaf.as_str(), None, 3)])
    {
        sqlx::query(
            "INSERT INTO mm_fleet_relay_feeds (stream_id, node_id, feeder_node_id, depth)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&b1)
        .bind(node)
        .bind(feeder)
        .bind(depth)
        .execute(&db)
        .await
        .expect("insert feed");
    }

    // Dead sources are replaced the first time they read dead.
    let ctx = context(addresses.clone()).with_inactive_grace(Duration::ZERO);
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));

    // Pass 1: every orphan's fallback is the entry, but only one is re-created from it.
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    let mut replaced = Vec::new();
    for orphan in &orphans {
        if removals_of(&calls[orphan], &source).await == 1 {
            replaced.push(orphan.clone());
        }
    }
    assert_eq!(
        replaced.len(),
        1,
        "exactly one orphan is re-created: {replaced:?}"
    );
    let winner = replaced[0].clone();
    assert_eq!(
        seeded_from(&calls[&winner], &source).await.last(),
        Some(&json!(origin_url))
    );
    for orphan in orphans.iter().filter(|o| **o != winner) {
        assert_eq!(
            removals_of(&calls[orphan], &source).await,
            0,
            "{orphan} is left as it is this tick"
        );
        assert!(calls[orphan].lock().await.held.contains(&source));
    }
    assert_eq!(
        removals_of(&calls[&leaf], &source).await,
        1,
        "a row recorded as fed by the entry is re-created at once"
    );
    assert_eq!(
        seeded_from(&calls[&leaf], &source).await.last(),
        Some(&json!(origin_url))
    );
    let mut expected: Vec<(String, Option<String>, i16)> = orphans
        .iter()
        .filter(|o| **o != winner)
        .map(|o| (o.clone(), Some(dead_feeder.clone()), 2))
        .chain([(winner.clone(), None, 1), (leaf.clone(), None, 1)])
        .collect();
    expected.sort();
    assert_eq!(feeds(&db, &b1).await, expected);

    // The winner's new source has the picture.
    calls[&winner].lock().await.dead = false;
    calls[&leaf].lock().await.dead = false;

    // Pass 2: the other two take the winner, now a stored first-level relay, as their
    // fallback, and are re-created from it.
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;
    let winner_url = &addresses[&format!(
        "192.0.2.{}",
        10 + orphans
            .iter()
            .position(|o| *o == winner)
            .expect("an orphan")
    )];
    for orphan in orphans.iter().filter(|o| **o != winner) {
        assert_eq!(removals_of(&calls[orphan], &source).await, 1, "{orphan}");
        assert_eq!(
            seeded_from(&calls[orphan], &source).await.last(),
            Some(&json!(winner_url)),
            "{orphan} is re-created from the winner, not the entry"
        );
    }
    let mut expected: Vec<(String, Option<String>, i16)> = orphans
        .iter()
        .filter(|o| **o != winner)
        .map(|o| (o.clone(), Some(winner.clone()), 2))
        .chain([(winner.clone(), None, 1), (leaf.clone(), None, 1)])
        .collect();
    expected.sort();
    assert_eq!(feeds(&db, &b1).await, expected);

    let nodes: Vec<&str> = orphans
        .iter()
        .map(String::as_str)
        .chain([leaf.as_str(), dead_feeder.as_str()])
        .collect();
    cleanup(&db, &b1, &nodes, &provider).await;
    sqlx::query("DELETE FROM mm_fleet_provider_zones WHERE provider_id = $1")
        .bind(&provider_b)
        .execute(&db)
        .await
        .expect("delete zone");
    sqlx::query("DELETE FROM mm_fleet_providers WHERE id = $1")
        .bind(&provider_b)
        .execute(&db)
        .await
        .expect("delete provider");
}

/// A relay that does not answer the census keeps the session count it had: writing 0 would
/// make a relay people may be on look empty, to the lease extension and to its headroom.
#[tokio::test]
async fn a_relay_that_does_not_answer_the_census_keeps_its_count() {
    let _serial = SERIAL.lock().await;
    let Some(db) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    mm_db::run_pg_migrations(&db)
        .await
        .expect("migrations should apply cleanly");

    let tag = uuid::Uuid::new_v4().simple().to_string();
    let (silent, answering) = (
        format!("0-feeds-{tag}-silent"),
        format!("0-feeds-{tag}-answering"),
    );
    let provider = format!("feeds-{tag}");
    let b1 = seed_active_stream(&db).await;
    let origin_url = fake_origin(json!(null), json!(null)).await;
    let (url, _) = fake_relay(json!([
        { "id": switch_viewer_id(&b1, "@bob:example.org"), "current_source": switch_source_id(&b1), "connected": true }
    ]))
    .await;

    seed_provider_zone(&db, &provider, "ams-1").await;
    // The silent relay's address maps to nothing that answers.
    seed_relay(&db, &silent, "192.0.2.20", &provider, "ams-1").await;
    seed_relay(&db, &answering, "192.0.2.21", &provider, "ams-1").await;
    sqlx::query("UPDATE mm_fleet_nodes SET viewers_current = 5 WHERE mm_node_id = ANY($1)")
        .bind(vec![silent.clone(), answering.clone()])
        .execute(&db)
        .await
        .expect("set counts");

    let ctx = context(HashMap::from([("192.0.2.21".to_string(), url)]));
    let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
        SwitchClient::new(&origin_url),
    )));
    mm_api::pool_sync::sync_once(&pool, &db, &ctx).await;

    let count = |id: String| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i32>(
                "SELECT viewers_current FROM mm_fleet_nodes WHERE mm_node_id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .expect("read count")
        }
    };
    assert_eq!(
        count(silent.clone()).await,
        5,
        "no answer: the stored count stays"
    );
    assert_eq!(
        count(answering.clone()).await,
        1,
        "an answer is written back"
    );

    cleanup(&db, &b1, &[&silent, &answering], &provider).await;
}

async fn seed_provider_zone(db: &sqlx::PgPool, provider: &str, zone: &str) {
    // A priority no other live provider holds (the column is unique among them).
    let priority = 1_000_000 + (uuid::Uuid::new_v4().as_u128() % 1_000_000_000) as i32;
    sqlx::query(
        "INSERT INTO mm_fleet_providers (id, label, kind, priority, endpoint_display, image, gpu_image)
         VALUES ($1, $1, 'scaleway', $2, 'test', 'img', 'gpu-img')",
    )
    .bind(provider)
    .bind(priority)
    .execute(db)
    .await
    .expect("insert provider");
    sqlx::query(
        "INSERT INTO mm_fleet_provider_zones (provider_id, zone, region, position) VALUES ($1, $2, 'eu', 0)",
    )
    .bind(provider)
    .bind(zone)
    .execute(db)
    .await
    .expect("insert zone");
}

async fn seed_relay(db: &sqlx::PgPool, id: &str, ip: &str, provider: &str, zone: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, public_ip,
                                     provider_ref, provider_zone, funding, viewer_capacity)
         VALUES ($1, 'fanout', 'owned', 'scaleway', 'healthy', $2::inet, $3, $4, 'operator', 250)",
    )
    .bind(id)
    .bind(ip)
    .bind(provider)
    .bind(zone)
    .execute(db)
    .await
    .expect("insert relay");
}

/// An active broadcast of its own, so other tests' streams never change its count.
async fn seed_active_stream(pool: &sqlx::PgPool) -> String {
    let db = PgDatabase::from_pool(pool.clone());
    let room = db
        .get_or_create_room(&RoomId(format!(
            "!feeds-{}:localhost",
            uuid::Uuid::new_v4()
        )))
        .await
        .expect("create room");
    db.create_stream(
        room.id,
        &UserId("@host:localhost".to_string()),
        Some("Relay feeds fixture"),
        "video",
        None,
        None,
        "core",
    )
    .await
    .expect("create stream")
    .id
}

async fn cleanup(db: &sqlx::PgPool, stream_id: &str, nodes: &[&str], provider: &str) {
    for node in nodes {
        for sql in [
            "DELETE FROM mm_fleet_relay_feeds WHERE node_id = $1",
            "DELETE FROM mm_relay_usage_samples WHERE mm_node_id = $1",
            "DELETE FROM mm_fleet_nodes WHERE mm_node_id = $1",
        ] {
            sqlx::query(sql).bind(node).execute(db).await.expect(sql);
        }
    }
    sqlx::query("DELETE FROM mm_fleet_provider_zones WHERE provider_id = $1")
        .bind(provider)
        .execute(db)
        .await
        .expect("delete zone");
    sqlx::query("DELETE FROM mm_fleet_providers WHERE id = $1")
        .bind(provider)
        .execute(db)
        .await
        .expect("delete provider");
    PgDatabase::from_pool(db.clone())
        .update_stream_status(&StreamId(stream_id.to_string()), StreamStatus::Ended)
        .await
        .expect("end stream");
}
