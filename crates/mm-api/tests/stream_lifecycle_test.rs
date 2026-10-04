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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::stream_lifecycle::{
    EndContext, MarkerContext, StreamSweeper, SweepPolicy, end_and_finalise_stream,
    finalize_stream_marker, republish_active_marker, sweep_tick,
};
use mm_core::config::{Config, MatrixConfig};
use mm_core::metrics::Metrics;
use mm_core::switch_client::{SwitchClient, switch_source_id};
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::models::Stream;
use mm_db::{Database, PgDatabase};
use mm_matrix::client::HomeserverClient;
use mm_matrix::events::StreamEventContent;
use mm_sfu::{
    CreateRoomRequest, EgressInfo, EgressStatus, ParticipantInfo, ParticipantPermissions,
    RoomStats, SfuAdapter, SfuError, SfuRoom, SfuToken,
};

// ---------------------------------------------------------------------------
// Postgres harness (env-gated, mirrors feed_api.rs)
// ---------------------------------------------------------------------------

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
    /// When `Some`, the stub supports egress and `list_egresses` answers this list.
    egresses: Option<Vec<EgressInfo>>,
    /// Where `stop_egress` and `delete_room` calls are written, with a DB snapshot each.
    journal: Option<(Journal, Observer)>,
}

impl StubSfu {
    fn empty() -> Self {
        Self {
            occupied: AtomicBool::new(false),
            egresses: None,
            journal: None,
        }
    }

    /// Journal `stop_egress` / `delete_room` calls, each with `observer`'s DB snapshot.
    fn journaled(mut self, journal: &Journal, observer: &Observer) -> Self {
        self.journal = Some((journal.clone(), observer.clone()));
        self
    }

    /// Claim egress support; `list_egresses` answers `egresses`.
    fn with_egresses(mut self, egresses: Vec<EgressInfo>) -> Self {
        self.egresses = Some(egresses);
        self
    }

    fn set_occupied(&self, occupied: bool) {
        self.occupied.store(occupied, Ordering::SeqCst);
    }

    async fn note(&self, call: String) {
        if let Some((journal, observer)) = &self.journal {
            journal.push(format!("{call} | {}", observer.snapshot().await));
        }
    }
}

fn egress(id: &str, status: EgressStatus) -> EgressInfo {
    EgressInfo {
        egress_id: id.to_string(),
        status,
        room_name: "room".to_string(),
        started_at: None,
        output_url: None,
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
    async fn delete_room(&self, sfu_room_id: &str) -> Result<(), SfuError> {
        self.note(format!("delete_room {sfu_room_id}")).await;
        Ok(())
    }
    fn supports_egress(&self) -> bool {
        self.egresses.is_some()
    }
    async fn list_egresses(&self, _room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        Ok(self.egresses.clone().unwrap_or_default())
    }
    async fn stop_egress(&self, egress_id: &str) -> Result<(), SfuError> {
        self.note(format!("stop_egress {egress_id}")).await;
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
// Call journal + DB observer (step-order assertions)
// ---------------------------------------------------------------------------

/// The media-plane calls of an end, in the order the stubs received them.
#[derive(Clone, Default)]
struct Journal(Arc<StdMutex<Vec<String>>>);

impl Journal {
    fn push(&self, entry: String) {
        self.0.lock().unwrap().push(entry);
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Reads one stream's DB state at the moment a stub is called, so a journal entry shows
/// which DB steps had already run: `stream=<status> switch_rec=<status>/<mp4_status>`.
#[derive(Clone)]
struct Observer {
    pool: PgPool,
    stream_id: String,
}

impl Observer {
    async fn snapshot(&self) -> String {
        let stream: String = sqlx::query_scalar("SELECT status FROM mm_streams WHERE id = $1")
            .bind(&self.stream_id)
            .fetch_one(&self.pool)
            .await
            .expect("observe stream");
        let rec: Option<(String, String)> = sqlx::query_as(
            "SELECT status, mp4_status FROM mm_recordings \
             WHERE stream_id = $1 AND egress_id LIKE 'mm-switch:%'",
        )
        .bind(&self.stream_id)
        .fetch_optional(&self.pool)
        .await
        .expect("observe recording");
        let rec = rec.map_or("no-row".to_string(), |(s, mp4)| format!("{s}/{mp4}"));
        format!("stream={stream} switch_rec={rec}")
    }
}

// ---------------------------------------------------------------------------
// Stub mm-switch
// ---------------------------------------------------------------------------

struct StubSwitch {
    journal: Journal,
    observer: Option<Observer>,
}

/// Journals `record/finalise` and source removal; answers the reads the end path's
/// spawned MP4 tracker may make.
async fn switch_fallback(State(stub): State<Arc<StubSwitch>>, req: Request<Body>) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let snapshot = match &stub.observer {
        Some(observer) => format!(" | {}", observer.snapshot().await),
        None => String::new(),
    };

    if method == "POST"
        && let Some(source) = path
            .strip_prefix("/api/sources/")
            .and_then(|rest| rest.strip_suffix("/record/finalise"))
    {
        stub.journal.push(format!("record_finalise {source}{snapshot}"));
        return axum::Json(json!({ "id": source, "state": "finished" })).into_response();
    }
    if method == "DELETE"
        && let Some(source) = path.strip_prefix("/api/sources/")
    {
        stub.journal.push(format!("remove_source {source}{snapshot}"));
        return axum::Json(json!({ "ok": "true" })).into_response();
    }
    if method == "GET" && path.starts_with("/api/recordings/") {
        return axum::Json(json!({ "status": "pending" })).into_response();
    }
    stub.journal.push(format!("unexpected {method} {path}"));
    (StatusCode::NOT_FOUND, "not stubbed").into_response()
}

async fn spawn_switch(journal: &Journal, observer: Option<&Observer>) -> Arc<SwitchClient> {
    let stub = Arc::new(StubSwitch {
        journal: journal.clone(),
        observer: observer.cloned(),
    });
    let app = axum::Router::new()
        .fallback(switch_fallback)
        .with_state(stub);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub switch");
    let addr = listener.local_addr().expect("stub switch addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Arc::new(SwitchClient::new(&format!("http://{addr}")))
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// The shared end path's context over the test stubs. `public_url` feeds the
/// recording.available thumbnail hint.
fn end_ctx<'a>(
    marker: MarkerContext<'a>,
    sfu: &'a StubSfu,
    switch: Option<&'a Arc<SwitchClient>>,
    pool: Option<&'a PgPool>,
) -> EndContext<'a> {
    EndContext {
        marker,
        sfu,
        switch,
        pg_pool: pool,
        public_url: "https://mm.example",
    }
}

/// Liveness only (no duration cap), as every sweep test before the cap used it.
fn liveness(grace: Duration) -> SweepPolicy {
    SweepPolicy {
        grace: Some(grace),
        max_broadcast: None,
    }
}

/// An open mm-switch recording row, exactly as `start_recording` inserts it.
async fn seed_switch_recording(pool: &PgPool, stream: &Stream) -> String {
    let id = format!("{}_rec1", stream.id);
    sqlx::query(
        "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, \
         storage_key, storage_backend, mime_type, title, egress_id) \
         VALUES ($1, $2, $3, $4, 'recording', 'audio', $5, 'local', 'audio/webm', \
         'Recording: fixture', $6)",
    )
    .bind(&id)
    .bind(&stream.id)
    .bind(stream.room_id)
    .bind(&stream.host_user_id)
    .bind(format!("/data/recordings/{id}.webm"))
    .bind(format!("mm-switch:{}", switch_source_id(&stream.id)))
    .execute(pool)
    .await
    .expect("seed switch recording");
    id
}

/// An open LiveKit fallback recording row (egress path).
async fn seed_livekit_recording(pool: &PgPool, stream: &Stream, egress_id: &str) -> String {
    let id = format!("{}_seg1", stream.id);
    sqlx::query(
        "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, \
         storage_key, storage_backend, mime_type, egress_id) \
         VALUES ($1, $2, $3, $4, 'recording', 'audio', $5, 'local', 'audio/mp4', $6)",
    )
    .bind(&id)
    .bind(&stream.id)
    .bind(stream.room_id)
    .bind(&stream.host_user_id)
    .bind(format!("/data/recordings/{id}.mp4"))
    .bind(egress_id)
    .execute(pool)
    .await
    .expect("seed livekit recording");
    id
}

/// `(status, completed_at is set, mp4_status)` of one recording row.
async fn recording_state(pool: &PgPool, id: &str) -> (String, bool, String) {
    sqlx::query_as(
        "SELECT status, completed_at IS NOT NULL, mp4_status FROM mm_recordings WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read recording")
}

/// Move a stream's start into the past (the duration cap measures from `started_at`).
async fn backdate_start(pool: &PgPool, stream: &Stream, secs: i64) {
    sqlx::query("UPDATE mm_streams SET started_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(&stream.id)
        .bind(secs as f64)
        .execute(pool)
        .await
        .expect("backdate stream start");
}

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
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), None, liveness(Duration::from_secs(0)))
        .await;
    assert!(
        report.ended.contains(&stream.id),
        "first sweep with zero grace must end the stale stream: {report:?}"
    );
    assert_eq!(report.marker_failures, 0);

    // Second run: nothing left to end.
    let report2 = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), None, liveness(Duration::from_secs(0)))
        .await;
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
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), None, liveness(Duration::from_secs(600)))
        .await;
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
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), None, liveness(Duration::from_secs(600)))
        .await;
    assert!(report.ended.is_empty());
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active", "resumed stream must stay live");

    // Cleanup so later sweep tests start quiet.
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("cleanup");
}

/// The sweep's liveness rule counts a broadcast the switch carries as live: hosts publish
/// only to mm-switch, so LiveKit's room is empty (it answers `[]` for a room it does not
/// know) and the old LiveKit-only rule auto-ended every broadcast after the grace period.
#[tokio::test]
async fn sweep_does_not_end_an_empty_room_stream_the_switch_carries() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-switch-live").await;
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

    // (i) The switch lists an active `stream-{id}` source; the room is empty; grace is 0.
    let live: HashSet<String> = HashSet::from([switch_source_id(&stream.id)]);
    for _ in 0..2 {
        let report = sweeper
            .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), Some(&live), liveness(Duration::from_secs(0)))
            .await;
        assert_eq!(report.checked, 1);
        assert!(
            report.ended.is_empty(),
            "a broadcast the switch carries is live: {report:?}"
        );
    }
    assert_eq!(
        sweeper.tracked(),
        0,
        "a live broadcast keeps no emptiness clock"
    );
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active");
    assert!(stub.stream_state_puts().is_empty(), "no Matrix write");

    // (ii) The switch answered but does not carry THIS broadcast (another one is live):
    // the empty room is judged on its own and the stream is ended.
    let others: HashSet<String> = HashSet::from([switch_source_id("some-other-stream")]);
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), Some(&others), liveness(Duration::from_secs(0)))
        .await;
    assert!(
        report.ended.contains(&stream.id),
        "a live set without this stream's source must not protect it: {report:?}"
    );
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "ended");
}

/// (iii) `live_sources = None` — no switch configured, or its list was unavailable this tick —
/// falls back to the LiveKit-only rule: an empty room past the grace is ended. (A switch
/// outage longer than the grace period still ends broadcasts: the switch carries all media.)
#[tokio::test]
async fn sweep_without_a_switch_list_falls_back_to_the_livekit_rule() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-switch-none").await;
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

    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, None, Some(&pool)), None, liveness(Duration::from_secs(0)))
        .await;
    assert!(
        report.ended.contains(&stream.id),
        "no switch list: the LiveKit rule applies: {report:?}"
    );
    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "ended");
}

/// `sweep_tick` hands the tick's live set to the rule: a carried stream starts no clock, an
/// uncarried one does (inside the grace window nothing is ended either way).
#[tokio::test]
async fn sweep_tick_passes_the_live_sources_to_the_rule() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-tick-live").await;
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
    let mut cfg = Config::default();
    cfg.streaming.auto_end_grace_secs = 600;

    let live: HashSet<String> = HashSet::from([switch_source_id(&stream.id)]);
    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), Some(&live), &mut sweeper).await;
    assert!(report.ended.is_empty());
    assert_eq!(sweeper.tracked(), 0, "carried by the switch: no clock");

    let none_live: HashSet<String> = HashSet::new();
    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), Some(&none_live), &mut sweeper).await;
    assert!(report.ended.is_empty());
    assert_eq!(
        sweeper.tracked(),
        1,
        "not carried, empty room: the clock starts"
    );

    // The switch carries it again (host resumed): the clock is cleared.
    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), Some(&live), &mut sweeper).await;
    assert!(report.ended.is_empty());
    assert_eq!(sweeper.tracked(), 0);

    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active");
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("cleanup");
}

/// Flow: a fully paused sweep tick (`streaming.auto_end_grace_secs == 0` AND
/// `streaming.max_broadcast_secs == 0`) must be a pure early return — it never lists
/// active streams, never touches the DB, and never ends anything.
#[tokio::test]
async fn sweep_tick_off_never_touches_the_db() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-off").await;
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

    let mut cfg = Config::default();
    cfg.streaming.auto_end_grace_secs = 0;
    cfg.streaming.max_broadcast_secs = 0;

    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), None, &mut sweeper).await;
    assert_eq!(
        report.checked, 0,
        "a paused sweep must not list/examine any streams"
    );
    assert!(report.ended.is_empty());

    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(row.status, "active", "a paused sweep must never end a stream");

    // Cleanup so later sweep tests start quiet.
    db.update_stream_status(&stream_id, StreamStatus::Ended)
        .await
        .expect("cleanup");
}

/// Flow: a clock started while the sweep is active must not survive a pause. Before
/// `sweep_tick` existed, the disabled path returned early without ever calling
/// `StreamSweeper::run_once`, so `empty_since` was never pruned — a clock started
/// before the pause stayed frozen for the whole paused interval. Un-pausing later
/// then saw that frozen clock as having elapsed the ENTIRE gap (paused time
/// included), which could auto-end a stream that had simply reconnected while the
/// sweep was off.
#[tokio::test]
async fn sweep_tick_off_resets_tracked_clocks() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-reset").await;
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

    // Active sweep, generous grace: the empty room only starts a clock — the
    // stream must not be touched.
    let mut cfg_on = Config::default();
    cfg_on.streaming.auto_end_grace_secs = 600;
    let report = sweep_tick(&cfg_on, &end_ctx(ctx, &sfu, None, Some(&pool)), None, &mut sweeper).await;
    assert!(report.ended.is_empty());
    assert_eq!(sweeper.tracked(), 1, "the empty room must start a clock");

    // Operator pauses the liveness rule: the clock must be CLEARED, not left frozen. The
    // duration cap keeps its default (12 h), so the tick still lists the streams — the
    // fresh one is under the cap and untouched. (Both rules off touches nothing at all:
    // `sweep_tick_off_never_touches_the_db`.)
    let mut cfg_off = Config::default();
    cfg_off.streaming.auto_end_grace_secs = 0;
    let report = sweep_tick(&cfg_off, &end_ctx(ctx, &sfu, None, Some(&pool)), None, &mut sweeper).await;
    assert!(report.ended.is_empty(), "{report:?}");
    assert_eq!(
        sweeper.tracked(),
        0,
        "a paused tick must reset tracked clocks, not leave them frozen"
    );

    let row = db.get_stream(&stream_id).await.unwrap().unwrap();
    assert_eq!(
        row.status, "active",
        "still active — never touched by the paused tick"
    );

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

// ---------------------------------------------------------------------------
// The shared end path (host end + sweep): media finalisation
// ---------------------------------------------------------------------------

/// Flow (the crashed-host bug): a stream the sweep auto-ends gets the same media
/// finalisation as a host end. Before, the sweep only flipped the DB row and wrote the
/// marker: the open mm-switch recording stayed `recording` (no `record/finalise`, so the
/// WebM had no trailer) and the dead `stream-{id}` source stayed on the switch.
#[tokio::test]
async fn sweep_end_finalises_the_switch_recording_and_removes_the_source() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "sweep-rec").await;
    let rec_id = seed_switch_recording(&pool, &stream).await;
    let source = switch_source_id(&stream.id);

    let journal = Journal::default();
    let observer = Observer { pool: pool.clone(), stream_id: stream.id.clone() };
    let switch = spawn_switch(&journal, Some(&observer)).await;
    let sfu = StubSfu::empty().journaled(&journal, &observer);

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext { hs_client: &client, db: &db, matrix: &matrix_cfg, metrics: &metrics };
    let mut sweeper = StreamSweeper::new();

    // The crashed host's switch source is inactive (not in the live set); grace 0.
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, Some(&switch), Some(&pool)), Some(&HashSet::new()), liveness(Duration::ZERO))
        .await;
    assert_eq!(report.ended, vec![stream.id.clone()], "{report:?}");

    // The switch closed the recording and dropped the dead source; the SFU room went last.
    assert_eq!(
        journal.entries(),
        vec![
            format!("record_finalise {source} | stream=active switch_rec=recording/none"),
            format!("remove_source {source} | stream=active switch_rec=ready/pending"),
            format!("delete_room sfu-sweep-rec | stream=active switch_rec=ready/pending"),
        ]
    );
    assert_eq!(
        recording_state(&pool, &rec_id).await,
        ("ready".to_string(), true, "pending".to_string()),
        "the recording is a VOD now, with its MP4 rendition tracked"
    );
    assert_eq!(db.get_stream(&StreamId(stream.id.clone())).await.unwrap().unwrap().status, "ended");

    // Newsfeed: the recording is announced like after a host end.
    assert!(
        stub.recorded().iter().any(|r| r.method == "PUT"
            && r.path.contains("/send/com.steegler.matrixmedia.feed.recording.available/")
            && r.body["recording_id"] == rec_id.as_str()),
        "feed recording.available must be emitted for the finalised recording"
    );
}

/// The shared end path keeps `end_stream`'s step order: LiveKit egress cleanup →
/// switch `record/finalise` → recording `ready` flip → MP4 tracking → switch source
/// removal → SFU room delete → stream status update. The journal's DB snapshot at each
/// call shows which DB steps had run.
#[tokio::test]
async fn the_shared_end_path_keeps_end_streams_step_order() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "end-order").await;
    let switch_rec = seed_switch_recording(&pool, &stream).await;
    let lk_rec = seed_livekit_recording(&pool, &stream, "EG_fallback").await;
    let source = switch_source_id(&stream.id);

    let journal = Journal::default();
    let observer = Observer { pool: pool.clone(), stream_id: stream.id.clone() };
    let switch = spawn_switch(&journal, Some(&observer)).await;
    // LiveKit lists the fallback egress (running) and an old one it already finished.
    let sfu = StubSfu::empty()
        .with_egresses(vec![
            egress("EG_fallback", EgressStatus::Active),
            egress("EG_old", EgressStatus::Complete),
        ])
        .journaled(&journal, &observer);

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext { hs_client: &client, db: &db, matrix: &matrix_cfg, metrics: &metrics };

    let outcome = end_and_finalise_stream(&end_ctx(ctx, &sfu, Some(&switch), Some(&pool)), &stream)
        .await
        .expect("the end succeeds");
    assert!(outcome.marker_written);

    assert_eq!(
        journal.entries(),
        vec![
            "stop_egress EG_fallback | stream=active switch_rec=recording/none".to_string(),
            format!("record_finalise {source} | stream=active switch_rec=recording/none"),
            format!("remove_source {source} | stream=active switch_rec=ready/pending"),
            "delete_room sfu-end-order | stream=active switch_rec=ready/pending".to_string(),
        ]
    );
    assert_eq!(db.get_stream(&StreamId(stream.id.clone())).await.unwrap().unwrap().status, "ended");
    assert_eq!(recording_state(&pool, &switch_rec).await, ("ready".to_string(), true, "pending".to_string()));
    // Only mm-switch recordings get an MP4 rendition.
    assert_eq!(recording_state(&pool, &lk_rec).await, ("ready".to_string(), true, "none".to_string()));
    assert_eq!(metrics.streams_ended_total.get(), 1);
}

/// With monetization off mm-core has no `pg_pool`, so `start_recording` writes no row —
/// but mm-switch still records. The end path must close the switch recorder anyway
/// (`record/finalise` is idempotent: 404 when nothing records) and drop the source.
#[tokio::test]
async fn the_switch_recorder_is_finalised_even_without_a_recording_row() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_matrix_room_id, stream) = seed_stream(&db, "no-pool").await;
    let source = switch_source_id(&stream.id);

    let journal = Journal::default();
    let switch = spawn_switch(&journal, None).await;
    let sfu = StubSfu::empty();

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext { hs_client: &client, db: &db, matrix: &matrix_cfg, metrics: &metrics };

    end_and_finalise_stream(&end_ctx(ctx, &sfu, Some(&switch), None), &stream)
        .await
        .expect("the end succeeds");

    assert_eq!(
        journal.entries(),
        vec![format!("record_finalise {source}"), format!("remove_source {source}")]
    );
    assert_eq!(db.get_stream(&StreamId(stream.id.clone())).await.unwrap().unwrap().status, "ended");
}

// ---------------------------------------------------------------------------
// Maximum broadcast duration (streaming.max_broadcast_secs)
// ---------------------------------------------------------------------------

/// A broadcast older than the cap is ended even though it is live (the switch carries
/// it), through the shared end path — so its recording is finalised too. A live
/// broadcast under the cap is untouched.
#[tokio::test]
async fn sweep_ends_a_live_broadcast_past_the_maximum_duration() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_r1, old) = seed_stream(&db, "cap-old").await;
    let (_r2, young) = seed_stream(&db, "cap-young").await;
    backdate_start(&pool, &old, 3 * 3600).await;
    backdate_start(&pool, &young, 3600).await;
    seed_switch_recording(&pool, &old).await;

    let journal = Journal::default();
    let switch = spawn_switch(&journal, None).await;
    let sfu = StubSfu::empty();
    let live: HashSet<String> =
        HashSet::from([switch_source_id(&old.id), switch_source_id(&young.id)]);

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext { hs_client: &client, db: &db, matrix: &matrix_cfg, metrics: &metrics };
    let mut sweeper = StreamSweeper::new();

    let policy = SweepPolicy {
        grace: Some(Duration::from_secs(600)),
        max_broadcast: Some(Duration::from_secs(2 * 3600)),
    };
    let report = sweeper
        .run_once(&end_ctx(ctx, &sfu, Some(&switch), Some(&pool)), Some(&live), policy)
        .await;

    assert_eq!(report.ended, vec![old.id.clone()], "{report:?}");
    assert_eq!(report.over_max_duration, vec![old.id.clone()]);
    assert!(
        journal.entries().contains(&format!("record_finalise {}", switch_source_id(&old.id))),
        "the capped broadcast's recording is finalised: {:?}",
        journal.entries()
    );
    assert_eq!(db.get_stream(&StreamId(young.id.clone())).await.unwrap().unwrap().status, "active");

    db.update_stream_status(&StreamId(young.id.clone()), StreamStatus::Ended)
        .await
        .expect("cleanup");
}

/// The cap is its own rule: it still applies while the liveness sweep is paused
/// (`auto_end_grace_secs = 0`), and `max_broadcast_secs = 0` means no limit.
#[tokio::test]
async fn the_duration_cap_applies_while_the_liveness_sweep_is_off() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lifecycle_lock().lock().await;
    quiesce_active_streams(&pool).await;

    let (_stub, base_url) = spawn_stub(StatePutMode::AlwaysOk).await;
    let db = PgDatabase::from_pool(pool.clone());
    let (_r, stream) = seed_stream(&db, "cap-sweep-off").await;
    backdate_start(&pool, &stream, 2 * 3600).await;
    let stream_id = StreamId(stream.id.clone());

    let client = hs_client(&base_url);
    let matrix_cfg = test_matrix_config(&base_url);
    let metrics = Metrics::new();
    let ctx = MarkerContext { hs_client: &client, db: &db, matrix: &matrix_cfg, metrics: &metrics };
    let sfu = StubSfu::empty();
    let mut sweeper = StreamSweeper::new();

    // Liveness off and no cap: nothing happens (the empty room is not judged at all).
    let mut cfg = Config::default();
    cfg.streaming.auto_end_grace_secs = 0;
    cfg.streaming.max_broadcast_secs = 0;
    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), None, &mut sweeper).await;
    assert!(report.ended.is_empty(), "{report:?}");
    assert_eq!(db.get_stream(&stream_id).await.unwrap().unwrap().status, "active");

    // Liveness off, cap 1 h: the 2 h old broadcast is ended by the cap alone.
    cfg.streaming.max_broadcast_secs = 3600;
    let report = sweep_tick(&cfg, &end_ctx(ctx, &sfu, None, Some(&pool)), None, &mut sweeper).await;
    assert_eq!(report.ended, vec![stream.id.clone()], "{report:?}");
    assert_eq!(report.over_max_duration, vec![stream.id.clone()]);
    assert_eq!(sweeper.tracked(), 0, "a paused liveness sweep keeps no clocks");
    assert_eq!(db.get_stream(&stream_id).await.unwrap().unwrap().status, "ended");
}
