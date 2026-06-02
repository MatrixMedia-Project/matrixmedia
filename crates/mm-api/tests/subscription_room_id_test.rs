//! Coverage for Stage A5: the subscription record carries room_id.
//!
//! As with creator_room_tiers_test, there is no `mm_api::test_setup()` /
//! fakestripe checkout harness in this repo, so we exercise the exact SQL the
//! `create_subscription` handler and the `handle_subscription_checkout_completed`
//! webhook run against a live PostgreSQL. Skips when MM_DATABASE_URL is unset.

use mm_db::{Database, PgDatabase};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use uuid::Uuid;

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

fn sub_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup(pool: &PgPool, creator: &str) {
    sqlx::query("DELETE FROM mm_subscriptions WHERE creator_user_id = $1")
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

#[tokio::test]
async fn test_create_subscription_persists_room_id() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_create_subscription_persists_room_id");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = sub_lock().lock().await;

    let creator = "@creator_sub_room:s";
    let subscriber = "@sub_room:s";
    cleanup(&pool, creator).await;

    let db = PgDatabase::from_pool(pool.clone());
    let tier = db
        .create_subscription_tier(creator, Some("!room_sub:s"), 1, "Insider", 999, None, None, None)
        .await
        .expect("create tier");

    let sub_id = Uuid::new_v4();
    let session_id = format!("cs_test_{sub_id}");
    let period_end = chrono::Utc::now() + chrono::Duration::days(30);

    // Mirror of the create_subscription INSERT (room-scoped).
    sqlx::query(
        "INSERT INTO mm_subscriptions
            (id, subscriber_user_id, creator_user_id, room_id, tier_id, status,
             stripe_subscription_id, current_period_end, created_at)
         VALUES ($1, $2, $3, $4, $5, 'incomplete', $6, $7, now())",
    )
    .bind(sub_id)
    .bind(subscriber)
    .bind(creator)
    .bind(Some("!room_sub:s"))
    .bind(tier.id)
    .bind(&session_id)
    .bind(period_end)
    .execute(&pool)
    .await
    .expect("insert subscription");

    // The subscription row carries the room_id from the request.
    let got = db
        .get_subscription(subscriber, creator)
        .await
        .expect("get_subscription")
        .expect("subscription exists");
    assert_eq!(got.room_id.as_deref(), Some("!room_sub:s"));

    cleanup(&pool, creator).await;
}

#[tokio::test]
async fn test_webhook_backfills_room_id_from_metadata() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_webhook_backfills_room_id_from_metadata");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = sub_lock().lock().await;

    let creator = "@creator_backfill:s";
    let subscriber = "@sub_backfill:s";
    cleanup(&pool, creator).await;

    let db = PgDatabase::from_pool(pool.clone());
    let tier = db
        .create_subscription_tier(creator, None, 1, "Fan", 500, None, None, None)
        .await
        .expect("create tier");

    let sub_id = Uuid::new_v4();
    let session_id = format!("cs_test_{sub_id}");
    let period_end = chrono::Utc::now() + chrono::Duration::days(30);

    // Insert a subscription WITHOUT room_id (simulates an older client).
    sqlx::query(
        "INSERT INTO mm_subscriptions
            (id, subscriber_user_id, creator_user_id, tier_id, status,
             stripe_subscription_id, current_period_end, created_at)
         VALUES ($1, $2, $3, $4, 'incomplete', $5, $6, now())",
    )
    .bind(sub_id)
    .bind(subscriber)
    .bind(creator)
    .bind(tier.id)
    .bind(&session_id)
    .bind(period_end)
    .execute(&pool)
    .await
    .expect("insert subscription");

    // Mirror of handle_subscription_checkout_completed UPDATE with metadata
    // backfill (COALESCE(room_id, $room)).
    sqlx::query(
        "UPDATE mm_subscriptions
         SET status = 'active',
             stripe_subscription_id = $1,
             current_period_end = $2,
             room_id = COALESCE(room_id, $4),
             updated_at = now()
         WHERE stripe_subscription_id = $3 AND status = 'incomplete'",
    )
    .bind(&session_id)
    .bind(period_end)
    .bind(&session_id)
    .bind(Some("!backfilled:s"))
    .execute(&pool)
    .await
    .expect("webhook update");

    let got = db
        .get_subscription(subscriber, creator)
        .await
        .expect("get_subscription")
        .expect("subscription exists");
    assert_eq!(got.status, "active");
    assert_eq!(
        got.room_id.as_deref(),
        Some("!backfilled:s"),
        "webhook must backfill room_id from metadata when NULL"
    );

    cleanup(&pool, creator).await;
}
