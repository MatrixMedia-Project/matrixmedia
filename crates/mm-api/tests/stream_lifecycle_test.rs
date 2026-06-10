//! Flow tests for Phase S stream-marker hardening
//! (`mm_api::stream_lifecycle`).
//!
//! `AppState` is impractical to construct in tests (30+ fields), so the
//! lifecycle functions are factored over `MarkerContext` — these tests drive
//! them against:
//!   * a real PostgreSQL (env-gated on `MM_DATABASE_URL`, skip when unset —
//!     same harness as `feed_api.rs` / `permissions_test.rs`),
//!   * a tiny in-test axum stub homeserver that records every request and
//!     can be scripted per test (403-once / always-500 / always-ok),
//!   * a stub `SfuAdapter` whose participant list is test-controlled.
//!
//! All tests share one Postgres, so they serialize on a file-level lock and
//! force-end any lingering active streams before running sweep assertions.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::Mutex;

use mm_api::stream_lifecycle::{
    MarkerContext, StreamSweeper, finalize_stream_marker, republish_active_marker,
};
use mm_core::config::MatrixConfig;
use mm_core::metrics::Metrics;
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::models::Stream;
use mm_db::{Database, PgDatabase};
use mm_matrix::client::HomeserverClient;
use mm_matrix::events::StreamEventContent;
use mm_sfu::{
    CreateRoomRequest, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter, SfuError,
    SfuRoom, SfuToken,
};

// ---------------------------------------------------------------------------
// Postgres harness (env-gated, mirrors feed_api.rs)
// ---------------------------------------------------------------------------

async fn try_pool() -> Option<PgPool> {
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .ok()
}

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

/// Serialize the tests in this file: the sweep iterates ALL active streams
/// in the shared DB, so concurrent tests would end each other's fixtures.
fn lifecycle_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Force-end every active stream so a sweep run only sees this test's
/// fixture (lingering rows from earlier tests/runs would pollute reports).
async fn quiesce_active_streams(pool: &PgPool) {
    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE status = 'active'")
        .execute(pool)
        .await
        .expect("quiesce active streams");
}

// ---------------------------------------------------------------------------
// Stub homeserver
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Value,
}

/// How the stub answers `PUT .../state/com.matrixmedia.stream/...`.
#[derive(Debug, Clone, Copy)]
enum StatePutMode {
    AlwaysOk,
    /// First stream-state PUT → 403 M_FORBIDDEN ("not in room"); later → ok.
    FailFirstThenOk,
    /// Every stream-state PUT → 500.
    AlwaysFail,
}

struct StubHomeserver {
    records: StdMutex<Vec<RecordedRequest>>,
    state_put_mode: StatePutMode,
    state_put_count: AtomicUsize,
    event_counter: AtomicUsize,
}

impl StubHomeserver {
    fn recorded(&self) -> Vec<RecordedRequest> {
        self.records.lock().unwrap().clone()
    }

    fn stream_state_puts(&self) -> Vec<RecordedRequest> {
        self.recorded()
            .into_iter()
            .filter(|r| r.method == "PUT" && r.path.contains("/state/com.matrixmedia.stream/"))
            .collect()
    }
}

async fn stub_fallback(State(stub): State<Arc<StubHomeserver>>, req: Request<Body>) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 1_000_000)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

    stub.records.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        body,
    });

    // Scripted stream-state PUT.
    if method == "PUT" && path.contains("/state/com.matrixmedia.stream/") {
        let n = stub.state_put_count.fetch_add(1, Ordering::SeqCst);
        let fail = match stub.state_put_mode {
            StatePutMode::AlwaysOk => false,
            StatePutMode::FailFirstThenOk => n == 0,
            StatePutMode::AlwaysFail => true,
        };
        if fail {
            let (status, errcode) = match stub.state_put_mode {
                StatePutMode::FailFirstThenOk => (StatusCode::FORBIDDEN, "M_FORBIDDEN"),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "M_UNKNOWN"),
            };
            return (
                status,
                axum::Json(json!({ "errcode": errcode, "error": "not in room" })),
            )
                .into_response();
        }
        let id = stub.event_counter.fetch_add(1, Ordering::SeqCst);
        return axum::Json(json!({ "event_id": format!("$stream-state-{id}:stub") }))
            .into_response();
    }

    // Synapse admin user login (ensure_bot_in_room step 1).
    if method == "POST" && path.contains("/_synapse/admin/v1/users/") && path.ends_with("/login") {
        return axum::Json(json!({ "access_token": "syn_user_tok" })).into_response();
    }
    // Invite + join (steps 2-3).
    if method == "POST" && path.ends_with("/invite") {
        return axum::Json(json!({})).into_response();
    }
    if method == "POST" && path.contains("/join") {
        return axum::Json(json!({ "room_id": "!stub:room" })).into_response();
    }
    // Power-levels read/write (step 4, best-effort).
    if path.contains("/state/m.room.power_levels") {
        if method == "GET" {
            return axum::Json(json!({ "users": {} })).into_response();
        }
        return axum::Json(json!({ "event_id": "$pl:stub" })).into_response();
    }
    // Any other state event (E2EE key clear, etc.).
    if method == "PUT" && path.contains("/state/") {
        return axum::Json(json!({ "event_id": "$other-state:stub" })).into_response();
    }
    // Timeline sends (feed broadcast.ended, notices).
    if method == "PUT" && path.contains("/send/") {
        let id = stub.event_counter.fetch_add(1, Ordering::SeqCst);
        return axum::Json(json!({ "event_id": format!("$timeline-{id}:stub") })).into_response();
    }

    axum::Json(json!({})).into_response()
}

/// Spawn the stub on an ephemeral port; returns its base URL + handle.
async fn spawn_stub(mode: StatePutMode) -> (Arc<StubHomeserver>, String) {
    let stub = Arc::new(StubHomeserver {
        records: StdMutex::new(Vec::new()),
        state_put_mode: mode,
        state_put_count: AtomicUsize::new(0),
        event_counter: AtomicUsize::new(0),
    });
    let app = axum::Router::new()
        .fallback(stub_fallback)
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub homeserver");
    let addr = listener.local_addr().expect("stub local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (stub, format!("http://{addr}"))
}

// ---------------------------------------------------------------------------
// Stub SFU
// ---------------------------------------------------------------------------

struct StubSfu {
    /// When true, `list_participants` reports an occupied room.
    occupied: AtomicBool,
}

impl StubSfu {
    fn empty() -> Self {
        Self {
            occupied: AtomicBool::new(false),
        }
    }

    fn set_occupied(&self, occupied: bool) {
        self.occupied.store(occupied, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SfuAdapter for StubSfu {
    fn name(&self) -> &str {
        "stub"
    }
    async fn health_check(&self) -> Result<(), SfuError> {
        Ok(())
    }
    async fn create_room(&self, req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
        Ok(SfuRoom {
            sfu_room_id: req.name.clone(),
            name: req.name,
            num_participants: 0,
        })
    }
    async fn delete_room(&self, _sfu_room_id: &str) -> Result<(), SfuError> {
        Ok(())
    }
    async fn generate_token(
        &self,
        _room: &SfuRoom,
        _participant: &ParticipantInfo,
        _permissions: ParticipantPermissions,
    ) -> Result<SfuToken, SfuError> {
        Ok(SfuToken {
            token: "stub-token".into(),
            url: "ws://stub".into(),
        })
    }
    async fn remove_participant(
        &self,
        _sfu_room_id: &str,
        _participant_id: &str,
    ) -> Result<(), SfuError> {
        Ok(())
    }
    async fn list_participants(
        &self,
        sfu_room_id: &str,
    ) -> Result<Vec<ParticipantInfo>, SfuError> {
        if self.occupied.load(Ordering::SeqCst) {
            Ok(vec![ParticipantInfo {
                sfu_participant_id: "p1".into(),
                identity: "@host:localhost".into(),
                name: None,
            }])
        } else {
            // A crashed host's room eventually disappears from LiveKit:
            // "room not found" must count as empty.
            Err(SfuError::RoomNotFound(sfu_room_id.to_string()))
        }
    }
    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        Ok(RoomStats {
            sfu_room_id: sfu_room_id.to_string(),
            num_participants: 0,
            num_publishers: 0,
            num_subscribers: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn test_matrix_config(homeserver_url: &str) -> MatrixConfig {
    MatrixConfig {
        homeserver_url: homeserver_url.to_string(),
        server_name: "localhost".to_string(),
        synapse_admin_token: "test-synapse-admin".to_string(),
        as_token: "test-as-token".to_string(),
        ..MatrixConfig::default()
    }
}

fn hs_client(homeserver_url: &str) -> HomeserverClient {
    HomeserverClient::new(
        homeserver_url.to_string(),
        "test-as-token".to_string(),
        "@mmbot:localhost".to_string(),
    )
}

/// Create a room + active stream fixture; returns (matrix_room_id, stream).
async fn seed_stream(db: &PgDatabase, tag: &str) -> (String, Stream) {
    let matrix_room_id = format!("!marker-{tag}-{}:localhost", uuid::Uuid::new_v4());
    let room = db
        .get_or_create_room(&RoomId(matrix_room_id.clone()))
        .await
        .expect("create room");
    let stream = db
        .create_stream(
            room.id,
            &UserId("@host:localhost".to_string()),
            Some("Phase S fixture"),
            "audio",
            Some(&format!("sfu-{tag}")),
            None,
        )
        .await
        .expect("create stream");
    (matrix_room_id, stream)
}

// ---------------------------------------------------------------------------
// Flow tests
// ---------------------------------------------------------------------------

/// Flow: the terminal event is written even when the bot was missing from
/// the room — the first state PUT 403s, `ensure_bot_in_room` endpoints
/// succeed, and the retry lands the write.
#[tokio::test]
async fn terminal_event_written_despite_bot_missing_from_room() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;

    let (stub, base_url) = spawn_stub(StatePutMode::FailFirstThenOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (matrix_room_id, stream) = seed_stream(&db, "bot-missing").await;
    let stream_id = StreamId(stream.id.clone());

    // Mirror the end paths: DB transition happens before the Matrix write.
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("end stream in DB");

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext {
        hs_client: &client,
        db: &db,
        matrix: &matrix_cfg,
        metrics: &metrics,
    };

    let terminal = finalize_stream_marker(&ctx, &stream, &matrix_room_id).await;
    let event_id = terminal.expect("terminal event must be written after retry");

    // Two stream-state PUTs: the 403 and the successful retry.
    let puts = stub.stream_state_puts();
    assert_eq!(puts.len(), 2, "expected 403 then retry: {puts:?}");
    let final_body = &puts[1].body;
    assert_eq!(final_body["status"], "ended");
    assert_eq!(final_body["stream_id"], stream.id.as_str());
    assert_eq!(
        final_body["marker_generation"], 2,
        "create=1, terminal bump=2"
    );
    assert!(final_body["ended_at_ms"].as_i64().unwrap() > 0);

    // ensure_bot_in_room hit the admin-login/invite/join endpoints.
    let recorded = stub.recorded();
    assert!(
        recorded
            .iter()
            .any(|r| r.path.contains("/_synapse/admin/v1/users/") && r.path.ends_with("/login")),
        "ensure_bot_in_room admin login not hit"
    );
    assert!(recorded.iter().any(|r| r.path.ends_with("/invite")));
    assert!(recorded.iter().any(|r| r.path.contains("/join")));

    // Terminal event id persisted; failure metric untouched.
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.ended_event_id.as_deref(), Some(event_id.as_str()));
    assert_eq!(row.marker_generation, 2);
    assert_eq!(metrics.stream_terminal_events_total.get(), 1);
    assert_eq!(metrics.stream_terminal_event_failures_total.get(), 0);
}

/// Flow: permanent write failure is loud, not silent — 3 attempts, `None`,
/// failure metric incremented, and the DB end transition is NOT rolled back.
#[tokio::test]
async fn permanent_terminal_write_failure_is_loud_not_silent() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;

    let (stub, base_url) = spawn_stub(StatePutMode::AlwaysFail).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (matrix_room_id, stream) = seed_stream(&db, "perma-fail").await;
    let stream_id = StreamId(stream.id.clone());
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("end stream in DB");

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext {
        hs_client: &client,
        db: &db,
        matrix: &matrix_cfg,
        metrics: &metrics,
    };

    let terminal = finalize_stream_marker(&ctx, &stream, &matrix_room_id).await;
    assert!(terminal.is_none(), "permanent failure must return None");

    assert_eq!(
        stub.stream_state_puts().len(),
        3,
        "exactly three write attempts"
    );
    assert_eq!(metrics.stream_terminal_event_failures_total.get(), 1);
    assert_eq!(metrics.stream_terminal_events_total.get(), 0);

    // The Matrix failure must never roll back the DB end.
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "ended");
    assert!(row.ended_event_id.is_none());
}

/// Flow: the liveness sweep marks a stale stream ended, writes the terminal
/// marker, and emits feed.broadcast.ended.
#[tokio::test]
async fn sweep_marks_stale_stream_ended_and_writes_marker() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-stale").await;
    let stream_id = StreamId(stream.id.clone());

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext {
        hs_client: &client,
        db: &db,
        matrix: &matrix_cfg,
        metrics: &metrics,
    };
    let sfu = StubSfu::empty();
    let mut sweeper = StreamSweeper::new();

    // Grace forced to 0: the empty SFU room is already past the window.
    let report = sweeper.run_once(&ctx, &sfu, Duration::from_secs(0)).await;
    assert!(
        report.ended.contains(&stream.id),
        "first sweep with zero grace must end the stale stream: {report:?}"
    );
    assert_eq!(report.marker_failures, 0);

    // Second run: nothing left to end.
    let report2 = sweeper.run_once(&ctx, &sfu, Duration::from_secs(0)).await;
    assert!(report2.ended.is_empty());

    // DB: status flipped, ended_at set, terminal id persisted.
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "ended");
    assert!(row.ended_at.is_some(), "ended_at must be set");
    assert!(row.ended_event_id.is_some(), "terminal event id persisted");

    // Matrix: terminal marker PUT + feed broadcast.ended send recorded.
    let puts = stub.stream_state_puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].body["status"], "ended");
    assert_eq!(puts[0].body["stream_id"], stream.id.as_str());
    assert!(
        stub.recorded().iter().any(|r| r.method == "PUT"
            && r
                .path
                .contains("/send/com.steegler.matrixmedia.feed.broadcast.ended/")),
        "feed broadcast.ended must be emitted"
    );
    assert_eq!(metrics.stream_terminal_events_total.get(), 1);
}

/// Flow: the sweep respects the resume grace window — a stream inside the
/// window stays live with no Matrix write, an occupied SFU room clears the
/// clock, and a host resume republishes the active marker at generation 2.
#[tokio::test]
async fn sweep_respects_resume_grace_window() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (matrix_room_id, stream) = seed_stream(&db, "sweep-grace").await;
    let stream_id = StreamId(stream.id.clone());

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext {
        hs_client: &client,
        db: &db,
        matrix: &matrix_cfg,
        metrics: &metrics,
    };
    let sfu = StubSfu::empty();
    let mut sweeper = StreamSweeper::new();

    // Inside the 600s grace window: the empty room only starts the clock.
    let report = sweeper.run_once(&ctx, &sfu, Duration::from_secs(600)).await;
    assert!(report.ended.is_empty(), "grace window must protect the stream");
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active");
    assert!(
        stub.stream_state_puts().is_empty(),
        "no Matrix write inside the grace window"
    );

    // Host resumes: republish bumps marker_generation to 2.
    let base_content = StreamEventContent {
        stream_id: stream.id.clone(),
        status: "active".to_string(),
        host_user_id: stream.host_user_id.clone(),
        title: stream.title.clone(),
        media_type: stream.media_type.clone(),
        video_config: None,
        viewer_url: None,
        mm_server_url: None,
        mm_matrix_server: Some("localhost".to_string()),
        federation_enabled: Some(false),
        participant_count: 0,
        e2ee_enabled: None,
        e2ee_algorithm: None,
        e2ee_key_id: None,
        e2ee_key_generation: None,
        started_at_ms: 0,
        updated_at_ms: 0,
        marker_generation: 1,
    };
    let (event_id, generation) =
        republish_active_marker(&ctx, &stream, &matrix_room_id, base_content)
            .await
            .expect("republish must succeed");
    assert_eq!(generation, 2, "resume republish bumps the generation");

    let puts = stub.stream_state_puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].body["status"], "active");
    assert_eq!(puts[0].body["marker_generation"], 2);
    assert!(puts[0].body["updated_at_ms"].as_i64().unwrap() > 0);
    assert!(puts[0].body["started_at_ms"].as_i64().unwrap() > 0);

    // New state-event id persisted (push edge for "host is back").
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.state_event_id.as_deref(), Some(event_id.as_str()));
    assert_eq!(row.marker_generation, 2);

    // Host reconnected to the SFU: the sweep clears the emptiness clock and
    // never kills the resumed stream.
    sfu.set_occupied(true);
    let report = sweeper.run_once(&ctx, &sfu, Duration::from_secs(600)).await;
    assert!(report.ended.is_empty());
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active", "resumed stream must stay live");

    // Cleanup so later sweep tests start quiet.
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("cleanup");
}

/// Unit (DB-backed): marker_generation is strictly monotonic per stream.
#[tokio::test]
async fn marker_generation_bump_is_monotonic() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;

    let db = PgDatabase::from_pool(pool.clone());
    let (_room, stream) = seed_stream(&db, "generation").await;
    let stream_id = StreamId(stream.id.clone());

    assert_eq!(stream.marker_generation, 1, "create starts at generation 1");
    let g2 = db.bump_stream_marker_generation(&stream_id).await.unwrap();
    let g3 = db.bump_stream_marker_generation(&stream_id).await.unwrap();
    let g4 = db.bump_stream_marker_generation(&stream_id).await.unwrap();
    assert_eq!((g2, g3, g4), (2, 3, 4));

    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.marker_generation, 4);

    // Cleanup.
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("cleanup");
}
