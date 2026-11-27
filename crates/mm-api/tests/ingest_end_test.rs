//! Integration coverage for the internal ingest-end endpoint
//! (`POST /_mm/internal/v1/ingest/end`, `internal::end_ingest_stream`).
//!
//! Following the repo pattern (see `ingest_key_test.rs`), the full
//! `AppState` is not constructed in tests; these exercise the exact DB
//! calls the handler runs — `get_stream` (404 when absent) and
//! `end_stream_if_active`, the guarded DB transition inside the shared
//! end path (`stream_lifecycle::end_and_finalise_stream`) — against live
//! PostgreSQL. No-ops when `MM_DATABASE_URL` is unset. The WHIP-marker
//! refusal rule itself is pure and unit-tested in `internal.rs`
//! (`ingest_end_decision_ends_only_live_whip_streams`); the marker lives
//! in the room's `com.matrixmedia.stream` state event, not the DB.

use mm_db::Database as _;
use mm_db::test_support::require_or_try_pool as try_pool;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

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

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Mirror of the handler's DB-level flow: unknown → None (404);
/// live → first end transitions, repeat is an idempotent no-op.
#[tokio::test]
async fn ingest_end_transitions_once_and_is_idempotent() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let creator = "@ingest-end:test";
    let room_matrix = "!ingest-end:test";
    let url = std::env::var("MM_DATABASE_URL").expect("pool implies URL");
    let db = mm_db::PgDatabase::new(&url).await.expect("PgDatabase");

    sqlx::query("DELETE FROM mm_participants WHERE user_id = $1")
        .bind(creator)
        .execute(&pool)
        .await
        .expect("cleanup participants");
    sqlx::query("DELETE FROM mm_streams WHERE host_user_id = $1")
        .bind(creator)
        .execute(&pool)
        .await
        .expect("cleanup streams");

    // Unknown stream: the handler's 404.
    let unknown = db
        .get_stream(&mm_core::types::StreamId("no-such-stream".into()))
        .await
        .expect("get unknown");
    assert!(unknown.is_none(), "unknown stream must be absent (404)");

    // A live broadcast, as ensured by the resolve path.
    let room = db
        .get_or_create_room(&mm_core::types::RoomId(room_matrix.to_string()))
        .await
        .expect("get or create room");
    let stream = db
        .create_stream(
            room.id,
            &mm_core::types::UserId(creator.to_string()),
            None,
            "video",
            None,
            None,
            "core",
        )
        .await
        .expect("create stream");
    let sid = mm_core::types::StreamId(stream.id.clone());
    let fetched = db.get_stream(&sid).await.expect("get live").expect("live stream");
    assert_eq!(fetched.status, "active");
    assert!(fetched.ended_at.is_none());

    // First end transitions (the shared helper's guarded DB step).
    assert!(
        db.end_stream_if_active(&sid).await.expect("first end"),
        "first end must transition the stream"
    );
    let after = db.get_stream(&sid).await.expect("get ended").expect("ended stream");
    assert_ne!(after.status, "active");
    assert!(after.ended_at.is_some(), "ended_at must be stamped");

    // Repeat call: idempotent, no second transition ({ended:false}).
    assert!(
        !db.end_stream_if_active(&sid).await.expect("repeat end"),
        "repeat end must be an idempotent no-op"
    );

    sqlx::query("DELETE FROM mm_participants WHERE user_id = $1")
        .bind(creator)
        .execute(&pool)
        .await
        .expect("cleanup participants");
    sqlx::query("DELETE FROM mm_streams WHERE host_user_id = $1")
        .bind(creator)
        .execute(&pool)
        .await
        .expect("cleanup streams");
    sqlx::query("DELETE FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(room_matrix)
        .execute(&pool)
        .await
        .expect("cleanup room");
}
