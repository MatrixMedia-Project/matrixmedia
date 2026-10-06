//! Live streams are members-only: `mm_api::client::stream_visible_to`, the
//! check behind `POST /streams/{id}/join`, `GET /streams/{id}`,
//! `GET /streams/{id}/participants` and the fleet proxy's viewer routes.
//!
//! Runs against a real PostgreSQL (env-gated on `MM_DATABASE_URL`) and an
//! in-test stub of Synapse's admin `joined_rooms` endpoint that answers from a
//! per-user table, or fails. Every test uses its own room ids and MXIDs.

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

use mm_api::client::stream_visible_to;
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

#[derive(Default)]
struct StubSynapse {
    /// MXID → the rooms Synapse says it has joined.
    joined: StdMutex<HashMap<String, Vec<String>>>,
    /// Answer every lookup with a 500.
    fail: AtomicBool,
    requests: AtomicUsize,
}

async fn joined_rooms(State(stub): State<Arc<StubSynapse>>, Path(user_id): Path<String>) -> Response {
    stub.requests.fetch_add(1, Ordering::SeqCst);
    if stub.fail.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "synapse is down").into_response();
    }
    let rooms = stub.joined.lock().unwrap().get(&user_id).cloned().unwrap_or_default();
    axum::Json(json!({ "total": rooms.len(), "joined_rooms": rooms })).into_response()
}

struct Fixture {
    db: PgDatabase,
    stub: Arc<StubSynapse>,
    synapse_url: String,
    room: RoomId,
    host: UserId,
    stream: Stream,
}

async fn fixture(pool: &PgPool, test: &str) -> Fixture {
    let tag = format!("{test}-{}", uuid::Uuid::new_v4().simple());
    let db = PgDatabase::from_pool(pool.clone());
    let room = RoomId(format!("!room-{tag}:hs"));
    let host = UserId(format!("@host-{tag}:hs"));
    let row = db.get_or_create_room(&room).await.expect("create room");
    let stream = db
        .create_stream(row.id, &host, Some("private broadcast"), "video", None, None)
        .await
        .expect("create stream");

    let stub = Arc::new(StubSynapse::default());
    let app = Router::new()
        .route("/_synapse/admin/v1/users/{user_id}/joined_rooms", get(joined_rooms))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    Fixture { db, stub, synapse_url: format!("http://{addr}"), room, host, stream }
}

impl Fixture {
    async fn can_see(&self, caller: &UserId, token: &str) -> bool {
        stream_visible_to(
            &self.db,
            &reqwest::Client::new(),
            &self.synapse_url,
            token,
            &self.stream,
            caller,
        )
        .await
        .expect("stream_visible_to")
    }

    fn join(&self, user: &UserId, room: &str) {
        self.stub
            .joined
            .lock()
            .unwrap()
            .entry(user.0.clone())
            .or_default()
            .push(room.to_string());
    }

    fn requests(&self) -> usize {
        self.stub.requests.load(Ordering::SeqCst)
    }
}

fn someone(role: &str) -> UserId {
    UserId(format!("@{role}-{}:hs", uuid::Uuid::new_v4().simple()))
}

#[tokio::test]
async fn the_host_sees_their_stream_without_a_lookup() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_host_sees_their_stream_without_a_lookup");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "host").await;
    // Even with Synapse down and no membership on record.
    f.stub.fail.store(true, Ordering::SeqCst);

    assert!(f.can_see(&f.host, "admin-tok").await);
    assert_eq!(f.requests(), 0);
}

#[tokio::test]
async fn a_member_of_the_streams_room_sees_it() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_member_of_the_streams_room_sees_it");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "member").await;
    let member = someone("member");
    f.join(&member, &f.room.0);

    assert!(f.can_see(&member, "admin-tok").await);
}

#[tokio::test]
async fn holding_the_stream_id_is_not_enough() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping holding_the_stream_id_is_not_enough");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "outsider").await;
    let outsider = someone("outsider");
    f.join(&outsider, "!some-other-room:hs");

    assert!(!f.can_see(&outsider, "admin-tok").await);
    assert_eq!(f.requests(), 1, "the outsider's membership was looked up");
}

#[tokio::test]
async fn a_failed_membership_lookup_fails_closed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_membership_lookup_fails_closed");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "fail-closed").await;
    let member = someone("member");
    f.join(&member, &f.room.0);

    f.stub.fail.store(true, Ordering::SeqCst);
    assert!(!f.can_see(&member, "admin-tok").await, "Synapse erroring");

    f.stub.fail.store(false, Ordering::SeqCst);
    let before = f.requests();
    assert!(!f.can_see(&member, "").await, "no admin token configured");
    assert_eq!(f.requests(), before, "no token, no request");
}
