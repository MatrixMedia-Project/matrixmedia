//! Recordings are only listed for, and handed out to, their room's members.
//!
//! Drives `mm_api::client::room_recordings_visible_to` and
//! `recording_visible_to` (the bodies of `GET /rooms/{room_id}/recordings` and
//! `GET /recordings/{id}`) against a real PostgreSQL (env-gated on
//! `MM_DATABASE_URL`) and an in-test stub of Synapse's admin `joined_rooms`
//! endpoint that answers from a per-user table, or fails.
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
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::client::{recording_visible_to, room_recordings_visible_to};
use mm_core::types::{RoomId, UserId};
use mm_db::models::Recording;
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
    fn join(&self, user: &UserId, room: &RoomId) {
        self.joined
            .lock()
            .unwrap()
            .entry(user.0.clone())
            .or_default()
            .push(room.0.clone());
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
// Fixture: a room with two recordings by its host and three by a guest host,
// interleaved in time (newest first: guest, host, guest, host, guest)
// ---------------------------------------------------------------------------

struct Fixture {
    db: PgDatabase,
    stub: Arc<StubSynapse>,
    synapse_url: String,
    room: RoomId,
    host: UserId,
    /// The host's recordings, newest first.
    host_recs: Vec<Recording>,
    /// Every recording, newest first.
    all_recs: Vec<Recording>,
}

fn recording(room_id: i64, host: &UserId, minutes_ago: i64) -> Recording {
    let id = format!("rec_{}", uuid::Uuid::new_v4());
    Recording {
        id: id.clone(),
        stream_id: format!("stream-{id}"),
        room_id,
        host_user_id: host.0.clone(),
        status: "ready".to_string(),
        media_type: "video".to_string(),
        storage_key: format!("recordings/{id}.mp4"),
        storage_backend: "local".to_string(),
        mxc_url: None,
        cdn_url: Some(format!("https://cdn.example/{id}.mp4")),
        duration_ms: Some(1000),
        size_bytes: Some(2048),
        mime_type: "video/mp4".to_string(),
        sha256: None,
        title: Some("a broadcast".to_string()),
        egress_id: None,
        created_at: Utc::now() - Duration::minutes(minutes_ago),
        completed_at: None,
        min_tier_level: None,
        mp4_status: "none".to_string(),
        mp4_key: None,
    }
}

async fn fixture(pool: &PgPool, test: &str) -> Fixture {
    let tag = format!("{test}-{}", uuid::Uuid::new_v4().simple());
    let db = PgDatabase::from_pool(pool.clone());
    let room = RoomId(format!("!room-{tag}:hs"));
    let host = UserId(format!("@host-{tag}:hs"));
    let guest = UserId(format!("@guest-{tag}:hs"));
    let row = db.get_or_create_room(&room).await.expect("create room");

    let mut all_recs = Vec::new();
    for (who, minutes_ago) in [(&guest, 1), (&host, 2), (&guest, 3), (&host, 4), (&guest, 5)] {
        let r = recording(row.id, who, minutes_ago);
        db.create_recording(&r).await.expect("create recording");
        all_recs.push(r);
    }
    let host_recs = all_recs.iter().filter(|r| r.host_user_id == host.0).cloned().collect();

    let stub = Arc::new(StubSynapse::default());
    let synapse_url = serve(stub.clone()).await;
    Fixture { db, stub, synapse_url, room, host, host_recs, all_recs }
}

impl Fixture {
    async fn page(&self, caller: &UserId, token: &str, limit: u32, before: Option<&str>) -> Vec<String> {
        room_recordings_visible_to(
            &self.db,
            &reqwest::Client::new(),
            &self.synapse_url,
            token,
            &self.room,
            caller,
            limit,
            before,
        )
        .await
        .expect("room_recordings_visible_to")
        .expect("the room is known")
        .into_iter()
        .map(|r| r.id)
        .collect()
    }

    async fn can_see(&self, caller: &UserId, token: &str, rec: &Recording) -> bool {
        recording_visible_to(&self.db, &reqwest::Client::new(), &self.synapse_url, token, rec, caller)
            .await
            .expect("recording_visible_to")
    }
}

fn ids(recs: &[Recording]) -> Vec<String> {
    recs.iter().map(|r| r.id.clone()).collect()
}

fn someone(role: &str) -> UserId {
    UserId(format!("@{role}-{}:hs", uuid::Uuid::new_v4().simple()))
}

// ---------------------------------------------------------------------------
// GET /rooms/{room_id}/recordings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_caller_who_has_not_joined_the_room_lists_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_caller_who_has_not_joined_the_room_lists_nothing");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-outsider").await;
    let outsider = someone("outsider");
    f.stub.join(&outsider, &RoomId("!elsewhere:hs".into()));

    assert!(f.page(&outsider, "admin-tok", 20, None).await.is_empty());
}

#[tokio::test]
async fn a_member_lists_every_recording_in_the_room() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_member_lists_every_recording_in_the_room");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-member").await;
    let member = someone("member");
    f.stub.join(&member, &f.room);

    assert_eq!(f.page(&member, "admin-tok", 20, None).await, ids(&f.all_recs));
}

#[tokio::test]
async fn a_host_who_is_not_a_member_pages_through_only_their_own() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_host_who_is_not_a_member_pages_through_only_their_own");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-host").await;
    // The host left the room. Pages of one row, as a client paging with
    // before_id would ask: each page must hold the next of THEIR recordings,
    // never an empty page that strands the cursor.
    let first = f.page(&f.host, "admin-tok", 1, None).await;
    assert_eq!(first, vec![f.host_recs[0].id.clone()]);
    let second = f.page(&f.host, "admin-tok", 1, Some(&first[0])).await;
    assert_eq!(second, vec![f.host_recs[1].id.clone()]);
    let third = f.page(&f.host, "admin-tok", 1, Some(&second[0])).await;
    assert!(third.is_empty(), "nothing after their oldest");
}

#[tokio::test]
async fn a_failed_membership_lookup_lists_only_the_callers_own() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_membership_lookup_lists_only_the_callers_own");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-fail").await;
    let member = someone("member");
    f.stub.join(&member, &f.room);

    f.stub.fail.store(true, Ordering::SeqCst);
    assert!(f.page(&member, "admin-tok", 20, None).await.is_empty());
    assert_eq!(f.page(&f.host, "admin-tok", 20, None).await, ids(&f.host_recs));

    f.stub.fail.store(false, Ordering::SeqCst);
    let before = f.stub.requests();
    assert!(f.page(&member, "", 20, None).await.is_empty(), "no admin token");
    assert_eq!(f.stub.requests(), before, "no token, no request");
}

#[tokio::test]
async fn listing_asks_synapse_only_when_membership_matters() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping listing_asks_synapse_only_when_membership_matters");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-no-lookup").await;

    // Unknown room: None (the handler's 404), no lookup.
    let unknown = room_recordings_visible_to(
        &f.db,
        &reqwest::Client::new(),
        &f.synapse_url,
        "admin-tok",
        &RoomId(format!("!never-seen-{}:hs", uuid::Uuid::new_v4().simple())),
        &f.host,
        20,
        None,
    )
    .await
    .expect("unknown room");
    assert!(unknown.is_none());

    // A page holding only the caller's own recordings: the newest host row
    // sits behind the newest guest row, so page from just past it.
    let own_page = f.page(&f.host, "admin-tok", 1, Some(&f.all_recs[0].id)).await;
    assert_eq!(own_page, vec![f.host_recs[0].id.clone()]);

    assert_eq!(f.stub.requests(), 0, "neither answer depended on membership");
}

// ---------------------------------------------------------------------------
// GET /recordings/{id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_recording_is_visible_to_its_host_and_members_only() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_single_recording_is_visible_to_its_host_and_members_only");
        return;
    };
    ensure_migrations(&pool).await;
    let f = fixture(&pool, "rec-single").await;
    let rec = &f.host_recs[0];
    let member = someone("member");
    let outsider = someone("outsider");
    f.stub.join(&member, &f.room);

    assert!(f.can_see(&f.host, "admin-tok", rec).await, "the host");
    assert_eq!(f.stub.requests(), 0, "the host needs no lookup");
    assert!(f.can_see(&member, "admin-tok", rec).await, "a member");
    assert!(!f.can_see(&outsider, "admin-tok", rec).await, "an outsider holding the id");

    f.stub.fail.store(true, Ordering::SeqCst);
    assert!(!f.can_see(&member, "admin-tok", rec).await, "a failed lookup fails closed");
    assert!(f.can_see(&f.host, "admin-tok", rec).await, "the host, even then");
}
