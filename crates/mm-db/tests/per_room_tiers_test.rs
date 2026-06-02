//! Integration tests for room-scoped subscription tiers (V025).
//!
//! Mirrors the feed_db / announcements harness: each test gets a PgPool from
//! `MM_DATABASE_URL`, applies migrations once, and runs under a process-global
//! mutex so parallel tests don't observe each other's writes. Tests no-op
//! (skip) when `MM_DATABASE_URL` is unset so `cargo test` is green without a
//! live PostgreSQL.

use mm_db::{Database, PgDatabase};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

async fn try_pool() -> Option<PgPool> {
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    Some(pool)
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

fn tier_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Remove any creator-owned tiers seeded by these tests (leaves the platform
/// defaults from V012 alone, which all have creator_user_id IS NULL).
async fn cleanup_creator_tiers(pool: &PgPool, creator: &str) {
    sqlx::query("DELETE FROM mm_subscription_tiers WHERE creator_user_id = $1")
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup tiers");
}

#[tokio::test]
async fn test_list_tiers_room_specific_overrides_creator_default() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_list_tiers_room_specific_overrides_creator_default");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = tier_lock().lock().await;

    let me = "@alice_room_tiers:s";
    cleanup_creator_tiers(&pool, me).await;

    let db = PgDatabase::from_pool(pool.clone());

    // Creator-default tier (room_id = NULL).
    db.create_subscription_tier(me, None, 1, "Fan", 500, None, None, None)
        .await
        .expect("create default tier");
    // Room-specific override for !fun.
    db.create_subscription_tier(me, Some("!fun:s"), 1, "Casual", 500, None, None, None)
        .await
        .expect("create room tier");

    // For !fun, only the room-specific tier appears.
    let fun = db.list_tiers_for_room(me, Some("!fun:s")).await.unwrap();
    assert_eq!(fun.len(), 1, "room with override returns only its tiers");
    assert_eq!(fun[0].name, "Casual");
    assert_eq!(fun[0].room_id.as_deref(), Some("!fun:s"));

    // For a different room with no override, fall back to the creator default.
    let other = db.list_tiers_for_room(me, Some("!biz:s")).await.unwrap();
    assert_eq!(other.len(), 1, "room without override falls back to default");
    assert_eq!(other[0].name, "Fan");
    assert!(other[0].room_id.is_none());

    // Passing None returns the creator-default ladder directly.
    let default = db.list_tiers_for_room(me, None).await.unwrap();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].name, "Fan");

    cleanup_creator_tiers(&pool, me).await;
}

#[tokio::test]
async fn test_delete_subscription_tier_removes_row() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_delete_subscription_tier_removes_row");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = tier_lock().lock().await;

    let me = "@bob_room_tiers:s";
    cleanup_creator_tiers(&pool, me).await;

    let db = PgDatabase::from_pool(pool.clone());
    let tier = db
        .create_subscription_tier(me, Some("!x:s"), 2, "Gold", 1000, None, None, None)
        .await
        .expect("create tier");

    db.delete_subscription_tier(tier.id)
        .await
        .expect("delete tier");

    let after = db.list_tiers_for_room(me, Some("!x:s")).await.unwrap();
    assert!(after.is_empty(), "deleted tier must not be listed");

    cleanup_creator_tiers(&pool, me).await;
}
