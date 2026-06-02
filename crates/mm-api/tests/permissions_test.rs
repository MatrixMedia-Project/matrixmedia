//! Integration coverage for Stage C — per-tier permissions.
//!
//! Mirrors the env-gated harness used by `creator_room_tiers_test.rs` /
//! `subscription_room_id_test.rs`: there is no `mm_api::test_setup()` and
//! `AppState` is too large to construct for handler tests, so these exercise
//! the real DB layer (`ensure_spectator_tier`) and the real gate resolver
//! (`tier_gate::resolve_with_pool`) against a live PostgreSQL. They no-op
//! (skip) when `MM_DATABASE_URL` is unset so `cargo test` is green without a
//! live DB.

use mm_api::middleware::tier_gate;
use mm_core::permissions::TierPermissions;
use mm_db::{Database, PgDatabase};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

async fn try_pool() -> Option<PgPool> {
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(2)
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

fn perm_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup(pool: &PgPool, creator: &str, subscriber: &str) {
    sqlx::query("DELETE FROM mm_subscriptions WHERE subscriber_user_id = $1 OR creator_user_id = $2")
        .bind(subscriber)
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup subs");
    sqlx::query("DELETE FROM mm_subscription_tiers WHERE creator_user_id = $1")
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup tiers");
}

/// C3: first creator touch of a room seeds a tier_level=0 "Spectator" row with
/// spectator (read+tip) permissions; the call is idempotent.
#[tokio::test]
async fn test_ensure_spectator_tier_seeds_idempotently() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_ensure_spectator_tier_seeds_idempotently");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = perm_lock().lock().await;

    let creator = "@alice_spectator:s";
    let room = "!spec_room:s";
    cleanup(&pool, creator, "@nobody:s").await;
    let db = PgDatabase::from_pool(pool.clone());

    db.ensure_spectator_tier(creator, room).await.expect("seed 1");
    // Idempotent: a second call must not error or duplicate.
    db.ensure_spectator_tier(creator, room).await.expect("seed 2");

    let (count, level, name, perms): (i64, i32, String, serde_json::Value) = sqlx::query_as(
        "SELECT count(*)::bigint, min(tier_level), min(name), min(permissions::text)::jsonb
           FROM mm_subscription_tiers
          WHERE creator_user_id = $1 AND room_id = $2 AND tier_level = 0",
    )
    .bind(creator)
    .bind(room)
    .fetch_one(&pool)
    .await
    .expect("read spectator");

    assert_eq!(count, 1, "exactly one spectator row");
    assert_eq!(level, 0);
    assert_eq!(name, "Spectator");
    let p: TierPermissions = serde_json::from_value(perms).unwrap();
    assert_eq!(p, TierPermissions::spectator_default());

    cleanup(&pool, creator, "@nobody:s").await;
}

/// C4: with only a Spectator tier present (no active subscription), the gate
/// resolves to spectator perms — can_read/can_tip true, can_send false.
#[tokio::test]
async fn test_resolve_spectator_when_no_subscription() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_resolve_spectator_when_no_subscription");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = perm_lock().lock().await;

    let creator = "@alice_resolve_spec:s";
    let subscriber = "@bob_resolve_spec:s";
    let room = "!resolve_spec:s";
    cleanup(&pool, creator, subscriber).await;
    let db = PgDatabase::from_pool(pool.clone());
    db.ensure_spectator_tier(creator, room).await.expect("seed");

    let perms = tier_gate::resolve_with_pool(&pool, subscriber, creator, room).await;
    assert!(perms.can_read, "spectator can read");
    assert!(perms.can_tip, "spectator can tip");
    assert!(!perms.can_send, "spectator cannot send");
    assert!(!perms.can_join_live, "spectator cannot join live");

    cleanup(&pool, creator, subscriber).await;
}

/// C4: an active subscription to a paid tier with full permissions resolves to
/// that tier's blob (can_send/can_join_live true).
#[tokio::test]
async fn test_resolve_uses_active_subscription_tier_permissions() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_resolve_uses_active_subscription_tier_permissions");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = perm_lock().lock().await;

    let creator = "@alice_resolve_paid:s";
    let subscriber = "@bob_resolve_paid:s";
    let room = "!resolve_paid:s";
    cleanup(&pool, creator, subscriber).await;
    let db = PgDatabase::from_pool(pool.clone());
    db.ensure_spectator_tier(creator, room).await.expect("seed spectator");

    // Create a paid tier with full permissions and an active subscription to it.
    let full = serde_json::to_value(TierPermissions::full_size_user_default()).unwrap();
    let tier_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO mm_subscription_tiers
            (creator_user_id, room_id, tier_level, name, price_cents, currency,
             perks_json, permissions, is_active)
         VALUES ($1, $2, 2, 'Advisor', 3500, 'usd', '[]'::jsonb, $3, true)
         RETURNING id",
    )
    .bind(creator)
    .bind(room)
    .bind(&full)
    .fetch_one(&pool)
    .await
    .expect("create paid tier");

    sqlx::query(
        "INSERT INTO mm_subscriptions
            (subscriber_user_id, creator_user_id, room_id, tier_id, status, current_period_end)
         VALUES ($1, $2, $3, $4, 'active', now() + interval '30 days')",
    )
    .bind(subscriber)
    .bind(creator)
    .bind(room)
    .bind(tier_id)
    .execute(&pool)
    .await
    .expect("create subscription");

    let perms = tier_gate::resolve_with_pool(&pool, subscriber, creator, room).await;
    assert!(perms.can_send, "advisor can send");
    assert!(perms.can_join_live, "advisor can join live");
    assert!(perms.can_watch_recordings, "advisor can watch recordings");
    assert!(!perms.can_manage_room, "advisor cannot manage room");

    cleanup(&pool, creator, subscriber).await;
}
