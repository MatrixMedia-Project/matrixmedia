//! `observe` + `build_view` against a stub mm-switch, a stub SFU and a real Postgres.
//!
//! Measured LiveKit behaviour the stubs model (livekit-server v1.9.1 and v1.12, `--dev`):
//! ListParticipants for a room LiveKit does not know answers 200 with an empty list — so a
//! switch-only broadcast reads as `RoomLookup::Participants(0)`, not as a failure.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::broadcast_servers::{
    ObserveDeps, ServerDetail, ServerKind, ServerStatus, SwitchObservation, Trackers, Warning,
    build_view, observe, observe_within,
};
use mm_api::stream_lifecycle::RoomLookup;
use mm_core::config::Config;
use mm_core::switch_client::{SwitchClient, switch_source_id, switch_viewer_id};
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::models::Stream;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};
use mm_sfu::{
    CreateRoomRequest, EgressInfo, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter,
    SfuError, SfuRoom, SfuToken,
};

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

/// `observe` lists ALL active streams, so tests in this file must not overlap.
fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn quiesce_active_streams(pool: &PgPool) {
    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE status = 'active'")
        .execute(pool)
        .await
        .expect("quiesce active streams");
}

async fn stub(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// How the stub SFU's `health_check` behaves.
#[derive(Clone, Copy)]
enum Health {
    Up,
    /// Answers with an error at once (LiveKit down).
    Down,
    /// Never answers within a test's time limit (LiveKit wedged).
    Hangs,
}

/// How the stub SFU answers `list_participants`.
#[derive(Clone, Copy)]
enum Rooms {
    /// An empty list for every room — what real LiveKit answers for a room it does not
    /// know, e.g. a broadcast published only to mm-switch. The production case.
    Empty,
    /// An error for every room (a circuit-open breaker, an SFU error): `RoomLookup::Failed`.
    Fail,
    /// Never answers within a test's time limit.
    Hang,
}

/// A stub SFU that counts the calls the collector must (not) make.
struct StubSfu {
    health: Health,
    rooms: Rooms,
    participant_calls: AtomicUsize,
    egress_calls: AtomicUsize,
}

impl StubSfu {
    fn new(health: Health, rooms: Rooms) -> Self {
        Self {
            health,
            rooms,
            participant_calls: AtomicUsize::new(0),
            egress_calls: AtomicUsize::new(0),
        }
    }
}

const HANG: Duration = Duration::from_secs(30);
const SHORT_LIMIT: Duration = Duration::from_millis(300);

#[async_trait::async_trait]
impl SfuAdapter for StubSfu {
    fn name(&self) -> &str {
        "stub"
    }
    async fn health_check(&self) -> Result<(), SfuError> {
        match self.health {
            Health::Up => Ok(()),
            Health::Down => Err(SfuError::ConnectionFailed("connection refused".into())),
            Health::Hangs => {
                tokio::time::sleep(HANG).await;
                Ok(())
            }
        }
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
            token: "stub".into(),
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
    async fn list_participants(&self, sfu_room_id: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
        self.participant_calls.fetch_add(1, Ordering::SeqCst);
        match self.rooms {
            Rooms::Empty => Ok(vec![]),
            Rooms::Fail => Err(SfuError::ConnectionFailed(format!(
                "{sfu_room_id}: refused"
            ))),
            Rooms::Hang => {
                tokio::time::sleep(HANG).await;
                Ok(vec![])
            }
        }
    }
    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        Err(SfuError::RoomNotFound(sfu_room_id.to_string()))
    }
    /// The collector must never ask: a per-room ListEgress answers 500 on a LiveKit without
    /// Redis — an outage for the breaker that also guards `create_room`.
    async fn list_egresses(&self, _room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
        self.egress_calls.fetch_add(1, Ordering::SeqCst);
        Err(SfuError::ConnectionFailed(
            "egress not connected (redis required)".into(),
        ))
    }
}

async fn seed_stream(db: &PgDatabase, tag: &str) -> Stream {
    let room = db
        .get_or_create_room(&RoomId(format!(
            "!bs-{tag}-{}:localhost",
            uuid::Uuid::new_v4()
        )))
        .await
        .expect("create room");
    db.create_stream(
        room.id,
        &UserId("@host:localhost".to_string()),
        Some("Broadcast servers fixture"),
        "video",
        Some(&format!("sfu-bs-{tag}")),
        None,
    )
    .await
    .expect("create stream")
}

async fn end_stream(db: &PgDatabase, stream_id: &str) {
    db.update_stream_status(&StreamId(stream_id.to_string()), StreamStatus::Ended)
        .await
        .unwrap();
}

async fn seed_recording(pool: &PgPool, stream: &Stream, status: &str, egress_id: Option<&str>) {
    sqlx::query(
        "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, \
         storage_key, storage_backend, mime_type, egress_id) \
         VALUES ($1, $2, $3, $4, $5, 'video', 'recordings/x.webm', 'local', 'video/webm', $6)",
    )
    .bind(format!("rec_{}", uuid::Uuid::new_v4()))
    .bind(&stream.id)
    .bind(stream.room_id)
    .bind(&stream.host_user_id)
    .bind(status)
    .bind(egress_id)
    .execute(pool)
    .await
    .expect("seed recording");
}

fn switch_router(stream_id: &str, viewers_status: StatusCode) -> Router {
    let sources =
        json!({"sources": [{"id": switch_source_id(stream_id), "type": "webrtc", "active": true}]});
    let viewers: Value = json!({"viewers": [
        {"id": switch_viewer_id(stream_id, "@v1:localhost"), "current_source": switch_source_id(stream_id), "connected": true},
        {"id": switch_viewer_id(stream_id, "@v2:localhost"), "current_source": switch_source_id(stream_id), "connected": true}
    ]});
    Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(json!({"status": "ok", "sources": 1, "viewers": 2, "recorders": {}}))
            }),
        )
        .route(
            "/api/sources",
            get(move || {
                let body = sources.clone();
                async move { axum::Json(body) }
            }),
        )
        .route(
            "/api/viewers",
            get(move || {
                let body = viewers.clone();
                async move { (viewers_status, axum::Json(body)) }
            }),
        )
}

#[tokio::test]
async fn a_switch_only_broadcast_reads_zero_participants_and_is_flagged() {
    // The production case: LiveKit answers the broadcast's unknown room with [].
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let stream_id = seed_stream(&db, "empty-room").await.id;
    let switch = SwitchClient::new(&stub(switch_router(&stream_id, StatusCode::OK)).await);
    let sfu = StubSfu::new(Health::Up, Rooms::Empty);
    let cfg = Config::default();
    assert!(
        cfg.streaming.auto_end_grace_secs > 0,
        "the sweep must be on"
    );

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &sfu,
        switch: Some(&switch),
        cfg: &cfg,
    })
    .await;
    let view = build_view(&obs, &mut Trackers::default());

    let b = view
        .broadcasts
        .iter()
        .find(|b| b.stream_id == stream_id)
        .expect("seeded broadcast listed");
    assert_eq!(b.switch_source, Some(true));
    assert_eq!(b.livekit_participants, Some(0));
    assert_eq!(b.warnings, vec![Warning::SweepSeesEmpty]);
    let lk = view
        .servers
        .iter()
        .find(|s| s.kind == ServerKind::Livekit)
        .unwrap();
    assert_eq!(lk.status, Some(ServerStatus::Ok));
    assert_eq!(
        lk.detail,
        Some(ServerDetail::Livekit {
            participants: Some(0),
            rooms_unavailable: 0
        })
    );
    assert_eq!(sfu.participant_calls.load(Ordering::SeqCst), 1);

    end_stream(&db, &stream_id).await;
}

#[tokio::test]
async fn a_switch_only_broadcast_whose_room_lookup_fails_is_counted_and_flagged() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let stream_id = seed_stream(&db, "switch-only").await.id;
    let switch = SwitchClient::new(&stub(switch_router(&stream_id, StatusCode::OK)).await);
    let cfg = Config::default();

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &StubSfu::new(Health::Up, Rooms::Fail),
        switch: Some(&switch),
        cfg: &cfg,
    })
    .await;
    let view = build_view(&obs, &mut Trackers::default());

    let s = view
        .servers
        .iter()
        .find(|s| s.kind == ServerKind::MmSwitch)
        .unwrap();
    assert_eq!(s.status, Some(ServerStatus::Ok));
    let b = view
        .broadcasts
        .iter()
        .find(|b| b.stream_id == stream_id)
        .expect("seeded broadcast listed");
    assert_eq!(b.switch_source, Some(true));
    assert_eq!(b.switch_viewers, Some(2));
    assert_eq!(b.livekit_participants, None);
    assert_eq!(b.warnings, vec![Warning::SweepSeesEmpty]);
    assert!(!serde_json::to_string(&view).unwrap().contains("viewer-"));

    end_stream(&db, &stream_id).await;
}

#[tokio::test]
async fn a_switch_that_rejects_the_viewer_list_is_degraded() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let stream_id = seed_stream(&db, "viewers-401").await.id;
    let switch =
        SwitchClient::new(&stub(switch_router(&stream_id, StatusCode::UNAUTHORIZED)).await);
    let cfg = Config::default();

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &StubSfu::new(Health::Up, Rooms::Empty),
        switch: Some(&switch),
        cfg: &cfg,
    })
    .await;
    let view = build_view(&obs, &mut Trackers::default());

    let s = view
        .servers
        .iter()
        .find(|s| s.kind == ServerKind::MmSwitch)
        .unwrap();
    assert_eq!(s.status, Some(ServerStatus::Degraded));
    let b = view
        .broadcasts
        .iter()
        .find(|b| b.stream_id == stream_id)
        .unwrap();
    assert_eq!(
        b.switch_viewers, None,
        "a 401 must not read as zero viewers"
    );

    end_stream(&db, &stream_id).await;
}

#[tokio::test]
async fn an_unreachable_switch_is_observed_with_its_error() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;

    let db = PgDatabase::from_pool(pool.clone());
    let switch = SwitchClient::new("http://127.0.0.1:1");
    let cfg = Config::default();
    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &StubSfu::new(Health::Up, Rooms::Empty),
        switch: Some(&switch),
        cfg: &cfg,
    })
    .await;
    assert!(
        matches!(&obs.switch, SwitchObservation::Unreachable { error, .. } if error.contains("switch health failed"))
    );
    assert!(obs.livekit.is_ok());
}

#[tokio::test]
async fn a_hung_switch_times_out_without_holding_the_snapshot() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;

    let db = PgDatabase::from_pool(pool.clone());
    let hung = Router::new().route(
        "/health",
        get(|| async {
            tokio::time::sleep(HANG).await;
            axum::Json(json!({"status": "ok", "sources": 0, "viewers": 0, "recorders": {}}))
        }),
    );
    let switch = SwitchClient::new(&stub(hung).await);
    let cfg = Config::default();

    let started = Instant::now();
    let obs = observe_within(
        ObserveDeps {
            db: &db,
            sfu: &StubSfu::new(Health::Up, Rooms::Empty),
            switch: Some(&switch),
            cfg: &cfg,
        },
        SHORT_LIMIT,
    )
    .await;
    let took = started.elapsed();

    assert!(took < Duration::from_secs(5), "observe took {took:?}");
    match &obs.switch {
        SwitchObservation::Unreachable { error, .. } => {
            assert_eq!(error, "switch /health timed out after 300 ms")
        }
        other => panic!("expected a timed-out switch, got {other:?}"),
    }
    // The other probes were not held up by the hung switch.
    assert!(obs.livekit.is_ok());
    assert!(obs.streams.is_ok());
    let view = build_view(&obs, &mut Trackers::default());
    let s = view
        .servers
        .iter()
        .find(|s| s.kind == ServerKind::MmSwitch)
        .unwrap();
    assert_eq!(s.status, Some(ServerStatus::Unreachable));
}

#[tokio::test]
async fn when_livekit_health_fails_no_room_is_looked_up() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let a = seed_stream(&db, "lk-down-a").await.id;
    let b = seed_stream(&db, "lk-down-b").await.id;
    let cfg = Config::default();

    for (health, expected_error) in [
        (Health::Down, "connection failed: connection refused"),
        (Health::Hangs, "LiveKit health check timed out after 300 ms"),
    ] {
        let sfu = StubSfu::new(health, Rooms::Empty);
        let started = Instant::now();
        let obs = observe_within(
            ObserveDeps {
                db: &db,
                sfu: &sfu,
                switch: None,
                cfg: &cfg,
            },
            SHORT_LIMIT,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(obs.livekit.as_ref().unwrap_err(), expected_error);
        assert_eq!(
            sfu.participant_calls.load(Ordering::SeqCst),
            0,
            "a failed LiveKit is not asked about rooms"
        );
        let streams = obs.streams.as_ref().expect("stream listing answered");
        for id in [&a, &b] {
            let s = streams.iter().find(|s| &s.stream.id == id).expect("seeded");
            assert_eq!(s.room, RoomLookup::Failed);
        }
        let view = build_view(&obs, &mut Trackers::default());
        let lk = view
            .servers
            .iter()
            .find(|s| s.kind == ServerKind::Livekit)
            .unwrap();
        assert_eq!(
            lk.detail,
            Some(ServerDetail::Livekit {
                participants: None,
                rooms_unavailable: 2
            })
        );
    }

    end_stream(&db, &a).await;
    end_stream(&db, &b).await;
}

#[tokio::test]
async fn a_room_lookup_that_times_out_stops_the_remaining_lookups() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let ids = [
        seed_stream(&db, "lk-hang-a").await.id,
        seed_stream(&db, "lk-hang-b").await.id,
        seed_stream(&db, "lk-hang-c").await.id,
    ];
    let sfu = StubSfu::new(Health::Up, Rooms::Hang);
    let cfg = Config::default();

    let started = Instant::now();
    let obs = observe_within(
        ObserveDeps {
            db: &db,
            sfu: &sfu,
            switch: None,
            cfg: &cfg,
        },
        SHORT_LIMIT,
    )
    .await;
    // One time limit for the first lookup, not one per broadcast.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(obs.livekit.is_ok());
    assert_eq!(sfu.participant_calls.load(Ordering::SeqCst), 1);
    let streams = obs.streams.as_ref().expect("stream listing answered");
    for id in &ids {
        let s = streams.iter().find(|s| &s.stream.id == id).expect("seeded");
        assert_eq!(s.room, RoomLookup::Failed);
    }

    for id in &ids {
        end_stream(&db, id).await;
    }
}

#[tokio::test]
async fn fallback_recordings_come_from_the_recording_rows_without_asking_livekit_egress() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let on_switch = seed_stream(&db, "rec-switch").await;
    seed_recording(&pool, &on_switch, "recording", Some("mm-switch:stream-x")).await;
    let on_egress = seed_stream(&db, "rec-egress").await;
    seed_recording(&pool, &on_egress, "paused", Some("EG_running")).await;
    // Closed rows and a row naming no egress job are not counted.
    seed_recording(&pool, &on_egress, "ready", Some("EG_finished")).await;
    seed_recording(&pool, &on_egress, "recording", None).await;
    let sfu = StubSfu::new(Health::Up, Rooms::Empty);
    let cfg = Config::default();

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &sfu,
        switch: None,
        cfg: &cfg,
    })
    .await;
    let view = build_view(&obs, &mut Trackers::default());

    let egress = view
        .servers
        .iter()
        .find(|s| s.kind == ServerKind::LivekitEgress)
        .unwrap();
    assert_eq!(
        egress.detail,
        Some(ServerDetail::Egress { active: Some(1) })
    );
    assert_eq!(
        sfu.egress_calls.load(Ordering::SeqCst),
        0,
        "the collector never calls ListEgress"
    );

    end_stream(&db, &on_switch.id).await;
    end_stream(&db, &on_egress.id).await;
}
