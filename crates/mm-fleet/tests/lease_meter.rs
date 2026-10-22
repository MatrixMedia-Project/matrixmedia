//! Relay-hour metering against the database (spec §8.2 R4, Task 9).

use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use mm_db::metering_db::PgMeteringDb;
use mm_fleet::lease_meter::sweep_relay_hours;
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<AsyncMutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| AsyncMutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}
fn file_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}
async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("door");
    for t in [
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_rate_card",
        "mm_broadcaster_wallet",
        "mm_relay_usage_samples",
        "mm_fleet_nodes",
        "mm_streams",
        "mm_rooms",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
    tx.commit().await.expect("commit");
}

async fn stream(pool: &PgPool, id: &str, host: &str) {
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(format!("!r-{id}:hs"))
        .execute(pool)
        .await
        .expect("room");
    let room_id: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(format!("!r-{id}:hs"))
        .fetch_one(pool)
        .await
        .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(id)
    .bind(room_id)
    .bind(host)
    .execute(pool)
    .await
    .expect("stream");
}

async fn wallet(pool: &PgPool, user: &str) {
    sqlx::query(
        "INSERT INTO mm_broadcaster_wallet (user_id, currency, balance_minor)
         VALUES ($1, 'usd', 100000) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("wallet");
}

async fn fanout_node(pool: &PgPool, id: &str, kind: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline)
         VALUES ($1, 'fanout', 'rented', $2, 'healthy', now() + interval '1 hour')",
    )
    .bind(id)
    .bind(kind)
    .execute(pool)
    .await
    .expect("node");
}

async fn sample(pool: &PgPool, node: &str, broadcast: &str, at: DateTime<Utc>, viewers: i32) {
    sqlx::query(
        "INSERT INTO mm_relay_usage_samples (mm_node_id, broadcast_id, sampled_at, viewers)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(node)
    .bind(broadcast)
    .bind(at)
    .bind(viewers)
    .execute(pool)
    .await
    .expect("sample");
}

async fn rows(pool: &PgPool, node: &str) -> Vec<(String, i64, String, String)> {
    sqlx::query_as(
        "SELECT unit, quantity_milli, payer, broadcast_id FROM mm_usage_events \
         WHERE mm_node_id = $1 ORDER BY id",
    )
    .bind(node)
    .fetch_all(pool)
    .await
    .expect("rows")
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!("MM_DATABASE_URL not set — skipping {}", stringify!($name));
                return;
            };
            let _guard = file_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// A relay that billed one wallet-covered broadcast for a whole minute-quantized
// period (`scaleway`, not hour-billed) meters once per period and never again for
// the same period, however many times the sweep is replayed (idempotency per
// (node, period), the DB-gated test the brief asks for).
pg_test!(meter_is_idempotent_per_period, pool, {
    stream(&pool, "b1", "@host:hs").await;
    wallet(&pool, "@host:hs").await;
    fanout_node(&pool, "bc-b1-fanout-0", "scaleway").await;

    // now() is aligned so "the period that just completed" is a clean, known minute.
    let now = DateTime::<Utc>::from_timestamp(1_700_000_120, 0).unwrap();
    let period_start = now - Duration::seconds(60);
    sample(&pool, "bc-b1-fanout-0", "b1", period_start + Duration::seconds(10), 5).await;
    sample(&pool, "bc-b1-fanout-0", "b1", period_start + Duration::seconds(30), 5).await;

    let meter = PgMeteringDb::new(pool.clone());
    let s1 = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s1.events_written, 1, "{:?}", s1.errors);

    let r = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "relay_hour");
    assert_eq!(r[0].2, "wallet");
    assert_eq!(r[0].3, "b1");
    // One minute = 16.67 milli-hours, half-up to 17, all of it to the one broadcast.
    assert_eq!(r[0].1, 17);

    // Samples for the metered period are gone (V052's retention note).
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_relay_usage_samples")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(left, 0);

    // A replay at the same `now` re-derives the same period and the same slice: the
    // idempotency key makes the insert a no-op, so nothing new is written.
    let s2 = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s2.events_written, 0, "a replay must write nothing new");
    let r2 = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r2.len(), 1, "no duplicate row from the replay");
});

// A broadcast with no wallet is billed to the house, not dropped and not refused.
pg_test!(an_uncovered_broadcast_is_billed_to_the_house, pool, {
    stream(&pool, "b2", "@host2:hs").await; // no wallet row for @host2:hs
    fanout_node(&pool, "bc-b2-fanout-0", "scaleway").await;

    let now = DateTime::<Utc>::from_timestamp(1_700_000_120, 0).unwrap();
    let period_start = now - Duration::seconds(60);
    sample(&pool, "bc-b2-fanout-0", "b2", period_start + Duration::seconds(10), 3).await;

    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);
    let r = rows(&pool, "bc-b2-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].2, "house");
});

// A relay that billed with no samples at all for the period still gets metered —
// a single house slice for the whole period, never silently skipped.
pg_test!(a_quiet_relay_still_bills_the_house_for_the_period, pool, {
    fanout_node(&pool, "bc-idle-fanout-0", "hetzner").await;
    let now = DateTime::<Utc>::from_timestamp(1_700_003_700, 0).unwrap(); // hour-aligned + 5m

    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);
    let r = rows(&pool, "bc-idle-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "relay_hour");
    assert_eq!(r[0].2, "house");
    assert_eq!(r[0].1, 1000, "hetzner is hour-quantized: the whole hour to the house");
});
