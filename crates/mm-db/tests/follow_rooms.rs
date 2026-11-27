//! Integration coverage for go-live follow rooms (V065).
//!
//! Exercises the `Database` follow-room / block / notify-prefs methods
//! against live PostgreSQL — the same SQL the discovery handlers and the
//! follow fan-out run. No-ops (skips) when `MM_DATABASE_URL` is unset,
//! per the repo pattern (see `ingest_keys.rs`).

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};
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

async fn cleanup(pool: &PgPool, creator: &str) {
    for table in [
        "mm_follow_rooms",
        "mm_creator_blocks",
        "mm_creator_notify_prefs",
        "mm_creator_follows",
    ] {
        let col = if table == "mm_creator_follows" {
            "creator_user_id"
        } else {
            "creator_user_id"
        };
        sqlx::query(&format!("DELETE FROM {table} WHERE {col} = $1"))
            .bind(creator)
            .execute(pool)
            .await
            .expect("cleanup");
    }
}

#[tokio::test]
async fn follow_room_block_and_prefs_lifecycle() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let db = PgDatabase::from_pool(pool.clone());
    let creator = "@follow-creator:test";
    let fan = "@follow-fan:test";
    cleanup(&pool, creator).await;

    // Follow + reverse lookup (the fan-out's follower list).
    db.follow_creator(fan, creator).await.expect("follow");
    let followers = db.get_creator_followers(creator).await.expect("followers");
    assert_eq!(followers.len(), 1);
    assert_eq!(followers[0].user_id, fan);

    // Room mapping: none yet, upsert, read back, replace, delete.
    assert!(db.get_follow_room(fan, creator).await.unwrap().is_none());
    let room = db
        .upsert_follow_room(fan, creator, "!room1:test")
        .await
        .expect("upsert room");
    assert_eq!(room.room_id, "!room1:test");
    let room = db
        .upsert_follow_room(fan, creator, "!room2:test")
        .await
        .expect("replace room");
    assert_eq!(room.room_id, "!room2:test");
    db.delete_follow_room(fan, creator)
        .await
        .expect("delete room");
    assert!(db.get_follow_room(fan, creator).await.unwrap().is_none());

    // Blocks: not blocked, block, blocked, unblock.
    assert!(!db.is_blocked_by(creator, fan).await.unwrap());
    db.block_user(creator, fan).await.expect("block");
    assert!(db.is_blocked_by(creator, fan).await.unwrap());
    db.unblock_user(creator, fan).await.expect("unblock");
    assert!(!db.is_blocked_by(creator, fan).await.unwrap());

    // Notify prefs: default available, then streamer turns history off.
    assert!(db.get_notify_history_visible(creator).await.unwrap());
    db.set_notify_history_visible(creator, false)
        .await
        .expect("set prefs");
    assert!(!db.get_notify_history_visible(creator).await.unwrap());

    cleanup(&pool, creator).await;
}
