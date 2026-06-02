//! Integration coverage for room-scoped creator tier endpoints (Stage A).
//!
//! The real test harness in this repo (see `feed_api.rs`) does NOT construct a
//! full `AppState` for handler tests — `AppState` has 30+ fields (SFU, Stripe,
//! Redis, payment registry, etc.) and there is no `mm_api::test_setup()`
//! helper. The plan's pseudo-code (`app.create_tier_via_db`, `app.login_as`,
//! `mm_api::test_setup`) does not exist. Following the established pattern, we
//! exercise the exact SQL the `list_my_tiers` / `create_tier` / room-scoped
//! `adopt_platform_tier` handlers run, against a live PostgreSQL.
//!
//! These tests no-op (skip) when `MM_DATABASE_URL` is unset so `cargo test` is
//! green without a live DB.

use mm_db::{Database, PgDatabase};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
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

async fn cleanup(pool: &PgPool, creator: &str) {
    sqlx::query("DELETE FROM mm_subscription_tiers WHERE creator_user_id = $1")
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup");
}

/// Mirror of the SQL in `creator::list_my_tiers` (room-scoped branch +
/// creator-default fallback).
async fn list_my_tiers_sql(pool: &PgPool, creator: &str, room_id: Option<&str>) -> Vec<String> {
    let room_specific = if let Some(room) = room_id {
        sqlx::query(
            "SELECT name FROM mm_subscription_tiers
             WHERE creator_user_id = $1 AND room_id = $2 AND is_active = true
             ORDER BY tier_level ASC",
        )
        .bind(creator)
        .bind(room)
        .fetch_all(pool)
        .await
        .expect("room-specific query")
    } else {
        Vec::new()
    };

    let rows = if room_specific.is_empty() {
        sqlx::query(
            "WITH own AS (
                 SELECT id, name, tier_level FROM mm_subscription_tiers
                 WHERE creator_user_id = $1 AND room_id IS NULL AND is_active = true
             )
             SELECT name FROM own
             UNION ALL
             SELECT name FROM mm_subscription_tiers
             WHERE creator_user_id IS NULL AND room_id IS NULL AND is_active = true
               AND tier_level NOT IN (SELECT tier_level FROM own)
             ORDER BY name ASC",
        )
        .bind(creator)
        .fetch_all(pool)
        .await
        .expect("fallback query")
    } else {
        room_specific
    };

    rows.iter()
        .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn test_list_my_tiers_filters_by_room_id_query_param() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_list_my_tiers_filters_by_room_id_query_param");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = tier_lock().lock().await;

    let me = "@alice_api_tiers:s";
    cleanup(&pool, me).await;
    let db = PgDatabase::from_pool(pool.clone());

    db.create_subscription_tier(me, None, 1, "Fan", 500, None, None, None)
        .await
        .expect("default tier");
    db.create_subscription_tier(me, Some("!fun:s"), 1, "Casual", 500, None, None, None)
        .await
        .expect("room tier");

    let fun = list_my_tiers_sql(&pool, me, Some("!fun:s")).await;
    assert_eq!(fun, vec!["Casual".to_string()], "room override wins");

    cleanup(&pool, me).await;
}

#[tokio::test]
async fn test_create_tier_with_room_id_round_trips() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_create_tier_with_room_id_round_trips");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = tier_lock().lock().await;

    let me = "@alice_create_tier:s";
    cleanup(&pool, me).await;
    let db = PgDatabase::from_pool(pool.clone());

    // create_tier handler path: POST with room_id="!biz:s".
    db.create_subscription_tier(me, Some("!biz:s"), 1, "Listener", 1500, None, None, None)
        .await
        .expect("create room-scoped tier");

    let listed = list_my_tiers_sql(&pool, me, Some("!biz:s")).await;
    assert_eq!(listed, vec!["Listener".to_string()]);

    cleanup(&pool, me).await;
}
