//! `GET /rooms/{room_id}/streams` only shows a room's streams to its members.
//!
//! Drives `mm_api::client::room_streams_visible_to` (the handler's body) against
//! a real PostgreSQL (env-gated on `MM_DATABASE_URL`, same harness as
//! `stream_lifecycle_test.rs`) and an in-test stub of Synapse's admin
//! `joined_rooms` endpoint that answers from a per-user table, or fails.
//!
//! Every test uses its own room ids and MXIDs, so rows left by other tests or
//! earlier runs never match.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::client::room_streams_visible_to;
use mm_core::types::{RoomId, UserId};
use mm_db::models::Stream;
use mm_db::{Database, PgDatabase};

use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
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

// ---------------------------------------------------------------------------
// Stub Synapse admin API
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubSynapse {
    /// MXID → the rooms Synapse says it has joined.
    joined: StdMutex<HashMap<String, Vec<String>>>,
    /// Answer every lookup with a 500.
    fail: AtomicBool,
    requests: AtomicUsize,
}

impl StubSynapse {
    fn join(&self, user: &str, room: &str) {
        self.joined
            .lock()
            .unwrap()
            .entry(user.to_string())
            .or_default()
            .push(room.to_string());
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn joined_rooms(State(stub): State<Arc<StubSynapse>>, Path(user_id): Path<String>) -> Response {
    stub.requests.fetch_add(1, Ordering::SeqCst);
    if stub.fail.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "synapse is down").into_response();
    }
    let rooms = stub.joined.lock().unwrap().get(&user_id).cloned().unwrap_or_default();
    axum::Json(json!({ "total": rooms.len(), "joined_rooms": rooms })).into_response()
}

async fn serve(stub: Arc<StubSynapse>) -> String {
    let app = Router::new()
        .route("/_synapse/admin/v1/users/{user_id}/joined_rooms", get(joined_rooms))
        .with_state(stub);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Fixture: one room with a stream by its host and one by a guest host
// ---------------------------------------------------------------------------

struct Fixture {
    db: PgDatabase,
    stub: Arc<StubSynapse>,
    synapse_url: String,
    room: RoomId,
    host: UserId,
    host_stream: Stream,
    guest_stream: Stream,
}

async fn fixture(pool: &PgPool, test: &str) -> Fixture {
    let tag = format!("{test}-{}", uuid::Uuid::new_v4().simple());
    let db = PgDatabase::from_pool(pool.clone());
    let room = RoomId(format!("!room-{tag}:hs"));
    let host = UserId(format!("@host-{tag}:hs"));
    let guest = UserId(format!("@guest-{tag}:hs"));

    let row = db.get_or_create_room(&room).await.expect("create room");
    let host_stream = db
        .create_stream(row.id, &host, Some("host's"), "video", None, None)
        .await
        .expect("host stream");
    db.end_stream_if_active(&mm_core::types::StreamId(host_stream.id.clone()))
        .await
        .expect("end host stream");
    let guest_stream = db
        .create_stream(row.id, &guest, Some("guest's"), "video", None, None)
        .await
        .expect("guest stream");

    let stub = Arc::new(StubSynapse::default());
    let synapse_url = serve(stub.clone()).await;
    Fixture { db, stub, synapse_url, room, host, host_stream, guest_stream }
}

async fn visible(f: &Fixture, caller: &UserId, admin_token: &str) -> Vec<String> {
    room_streams_visible_to(
        &f.db,
        &reqwest::Client::new(),
        &f.synapse_url,
        admin_token,
        &f.room,
        caller,
    )
    .await
    .expect("room_streams_visible_to")
    .into_iter()
    .map(|s| s.id)
    .collect()
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_caller_who_has_not_joined_the_room_gets_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_caller_who_has_not_joined_the_room_gets_nothing");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "outsider").await;
    let outsider = UserId(format!("@outsider-{}:hs", uuid::Uuid::new_v4().simple()));
    // Joined somewhere, just not here.
    f.stub.join(&outsider.0, "!elsewhere:hs");

    assert!(visible(&f, &outsider, "admin-tok").await.is_empty());
    assert_eq!(f.stub.requests(), 1, "the outsider's membership was looked up");
}

#[tokio::test]
async fn a_member_gets_every_stream_in_the_room() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_member_gets_every_stream_in_the_room");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "member").await;
    let member = UserId(format!("@member-{}:hs", uuid::Uuid::new_v4().simple()));
    f.stub.join(&member.0, &f.room.0);

    assert_eq!(
        sorted(visible(&f, &member, "admin-tok").await),
        sorted(vec![f.host_stream.id.clone(), f.guest_stream.id.clone()]),
        "live and ended streams, whoever hosted them"
    );
}

#[tokio::test]
async fn a_host_who_is_not_a_member_gets_only_their_own_streams() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_host_who_is_not_a_member_gets_only_their_own_streams");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "host").await;
    // The host left the room: Synapse no longer lists it for them.

    assert_eq!(visible(&f, &f.host, "admin-tok").await, vec![f.host_stream.id.clone()]);
}

#[tokio::test]
async fn a_failed_membership_lookup_fails_closed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_membership_lookup_fails_closed");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "fail-closed").await;
    let member = UserId(format!("@member-{}:hs", uuid::Uuid::new_v4().simple()));
    f.stub.join(&member.0, &f.room.0);

    // Synapse erroring: even a real member sees nothing of anyone else's.
    f.stub.fail.store(true, Ordering::SeqCst);
    assert!(visible(&f, &member, "admin-tok").await.is_empty());
    // The host still gets their own.
    assert_eq!(visible(&f, &f.host, "admin-tok").await, vec![f.host_stream.id.clone()]);

    // No admin token configured: the same, and Synapse is never asked.
    f.stub.fail.store(false, Ordering::SeqCst);
    let before = f.stub.requests();
    assert!(visible(&f, &member, "").await.is_empty());
    assert_eq!(f.stub.requests(), before, "no token, no request");
}

#[tokio::test]
async fn synapse_is_not_asked_when_membership_cannot_change_the_answer() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping synapse_is_not_asked_when_membership_cannot_change_the_answer");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "no-lookup").await;
    let anyone = UserId(format!("@anyone-{}:hs", uuid::Uuid::new_v4().simple()));

    // A room MM has never seen: empty, as before (not a 404).
    let unknown = room_streams_visible_to(
        &f.db,
        &reqwest::Client::new(),
        &f.synapse_url,
        "admin-tok",
        &RoomId(format!("!never-seen-{}:hs", uuid::Uuid::new_v4().simple())),
        &anyone,
    )
    .await
    .expect("unknown room");
    assert!(unknown.is_empty());

    // A room whose only stream is the caller's own.
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let solo_room = RoomId(format!("!solo-{tag}:hs"));
    let solo_host = UserId(format!("@solo-{tag}:hs"));
    let row = f.db.get_or_create_room(&solo_room).await.unwrap();
    let solo = f
        .db
        .create_stream(row.id, &solo_host, None, "video", None, None)
        .await
        .unwrap();
    let own = room_streams_visible_to(
        &f.db,
        &reqwest::Client::new(),
        &f.synapse_url,
        "admin-tok",
        &solo_room,
        &solo_host,
    )
    .await
    .expect("own room");
    assert_eq!(own.into_iter().map(|s| s.id).collect::<Vec<_>>(), vec![solo.id]);

    assert_eq!(f.stub.requests(), 0, "neither answer depended on membership");
}
