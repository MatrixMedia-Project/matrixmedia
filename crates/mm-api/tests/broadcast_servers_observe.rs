//! `observe` + `build_view` against a stub mm-switch, an SFU whose rooms are all gone
//! (what LiveKit reports for a switch-only broadcast) and a real Postgres.

use std::sync::OnceLock;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_api::broadcast_servers::{
    ObserveDeps, ServerKind, ServerStatus, SwitchObservation, Trackers, Warning, build_view,
    observe,
};
use mm_core::config::Config;
use mm_core::switch_client::{SwitchClient, switch_source_id, switch_viewer_id};
use mm_core::types::{RoomId, StreamId, StreamStatus, UserId};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};
use mm_sfu::{
    CreateRoomRequest, ParticipantInfo, ParticipantPermissions, RoomStats, SfuAdapter, SfuError,
    SfuRoom, SfuToken,
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

/// Every room is gone — exactly what LiveKit says about a broadcast published only to
/// mm-switch. `health_check` succeeds: LiveKit itself is up.
struct NoRoomsSfu;

#[async_trait::async_trait]
impl SfuAdapter for NoRoomsSfu {
    fn name(&self) -> &str {
        "no-rooms"
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
        Err(SfuError::RoomNotFound(sfu_room_id.to_string()))
    }
    async fn room_stats(&self, sfu_room_id: &str) -> Result<RoomStats, SfuError> {
        Err(SfuError::RoomNotFound(sfu_room_id.to_string()))
    }
}

async fn seed_stream(db: &PgDatabase, tag: &str) -> String {
    let room = db
        .get_or_create_room(&RoomId(format!(
            "!bs-{tag}-{}:localhost",
            uuid::Uuid::new_v4()
        )))
        .await
        .expect("create room");
    let stream = db
        .create_stream(
            room.id,
            &UserId("@host:localhost".to_string()),
            Some("Broadcast servers fixture"),
            "video",
            Some(&format!("sfu-bs-{tag}")),
            None,
        )
        .await
        .expect("create stream");
    stream.id
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
async fn a_switch_only_broadcast_is_counted_and_flagged() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = lock().lock().await;
    quiesce_active_streams(&pool).await;

    let db = PgDatabase::from_pool(pool.clone());
    let stream_id = seed_stream(&db, "switch-only").await;
    let switch = SwitchClient::new(&stub(switch_router(&stream_id, StatusCode::OK)).await);
    let cfg = Config::default();

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &NoRoomsSfu,
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

    db.update_stream_status(&StreamId(stream_id), StreamStatus::Ended)
        .await
        .unwrap();
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
    let stream_id = seed_stream(&db, "viewers-401").await;
    let switch =
        SwitchClient::new(&stub(switch_router(&stream_id, StatusCode::UNAUTHORIZED)).await);
    let cfg = Config::default();

    let obs = observe(ObserveDeps {
        db: &db,
        sfu: &NoRoomsSfu,
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

    db.update_stream_status(&StreamId(stream_id), StreamStatus::Ended)
        .await
        .unwrap();
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
        sfu: &NoRoomsSfu,
        switch: Some(&switch),
        cfg: &cfg,
    })
    .await;
    assert!(
        matches!(&obs.switch, SwitchObservation::Unreachable { error, .. } if error.contains("switch health failed"))
    );
    assert!(obs.livekit.is_ok());
}
