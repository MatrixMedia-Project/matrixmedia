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
use sqlx::{PgPool, Row};
use std::sync::OnceLock;
use tokio::sync::Mutex;

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

#[tokio::test]
async fn test_adopt_platform_tier_with_room_id_scopes_copy() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_adopt_platform_tier_with_room_id_scopes_copy");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = tier_lock().lock().await;

    let me = "@alice_adopt:s";
    cleanup(&pool, me).await;

    // Grab a platform-default tier id (seeded by V012, creator_user_id IS NULL).
    let platform_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM mm_subscription_tiers
         WHERE creator_user_id IS NULL AND room_id IS NULL AND is_active = true
         ORDER BY tier_level ASC LIMIT 1",
    )
    .fetch_optional(&pool)
    .await
    .expect("query platform tier");
    let Some(platform_id) = platform_id else {
        eprintln!("no platform-default tiers seeded — skipping adopt test");
        return;
    };

    // Mirror of adopt_platform_tier handler SQL with room_id = "!adopt:s".
    let row: Option<(uuid::Uuid, i32)> = sqlx::query_as(
        "INSERT INTO mm_subscription_tiers
            (creator_user_id, room_id, name, description, price_cents, currency,
             tier_level, perks_json, badge_url, is_active)
         SELECT $1, $3, name, description, price_cents, currency,
                tier_level, perks_json, badge_url, true
         FROM mm_subscription_tiers
         WHERE id = $2 AND creator_user_id IS NULL AND is_active = true
         ON CONFLICT (creator_user_id, COALESCE(room_id, ''), tier_level)
             WHERE creator_user_id IS NOT NULL
             DO NOTHING
         RETURNING id, tier_level",
    )
    .bind(me)
    .bind(platform_id)
    .bind("!adopt:s")
    .fetch_optional(&pool)
    .await
    .expect("adopt insert");

    assert!(row.is_some(), "adopt should insert a room-scoped copy");

    // The adopted copy is visible in the room's ladder.
    let listed = list_my_tiers_sql(&pool, me, Some("!adopt:s")).await;
    assert_eq!(listed.len(), 1, "adopted tier shows in room ladder");

    cleanup(&pool, me).await;
}
