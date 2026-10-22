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

async fn fanout_node(pool: &PgPool, id: &str, kind: &str, started: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, billing_started_at)
         VALUES ($1, 'fanout', 'rented', $2, 'healthy', now() + interval '1 hour', $3)",
    )
    .bind(id)
    .bind(kind)
    .bind(started)
    .execute(pool)
    .await
    .expect("node");
}

async fn mark_gone(pool: &PgPool, id: &str, gone_at: DateTime<Utc>) {
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone', gone_at = $2 WHERE mm_node_id = $1")
        .bind(id)
        .bind(gone_at)
        .execute(pool)
        .await
        .expect("gone");
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

async fn rows(pool: &PgPool, node: &str) -> Vec<(String, i64, String, String, String)> {
    sqlx::query_as(
        "SELECT unit, quantity_milli, payer, broadcast_id, user_id FROM mm_usage_events \
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
// window (`scaleway`, not hour-billed) meters once per window and never again for
// the same window, however many times the sweep is replayed (idempotency per
// (node, period), the DB-gated test the brief asks for). The window is anchored to
// billing_started_at, not wall-clock aligned.
pg_test!(meter_is_idempotent_per_period, pool, {
    stream(&pool, "b1", "@host:hs").await;
    wallet(&pool, "@host:hs").await;
    // Deliberately NOT minute-aligned: 1_700_000_013. The window is 013..073.
    let started = DateTime::<Utc>::from_timestamp(1_700_000_013, 0).unwrap();
    fanout_node(&pool, "bc-b1-fanout-0", "scaleway", started).await;
    sample(&pool, "bc-b1-fanout-0", "b1", started + Duration::seconds(10), 5).await;
    sample(&pool, "bc-b1-fanout-0", "b1", started + Duration::seconds(30), 5).await;

    let now = started + Duration::seconds(80); // inside the SECOND window
    let meter = PgMeteringDb::new(pool.clone());
    let s1 = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s1.events_written, 1, "{:?}", s1.errors);

    let r = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "relay_hour");
    assert_eq!(r[0].2, "wallet");
    assert_eq!(r[0].3, "b1");
    assert_eq!(r[0].4, "@host:hs", "a wallet slice bills the HOST, not the broadcast id");
    // One minute = 16.67 milli-hours, half-up to 17, all of it to the one broadcast.
    assert_eq!(r[0].1, 17);

    // The idempotency key is anchored to the node's own window start.
    let key: String = sqlx::query_scalar(
        "SELECT idempotency_key FROM mm_usage_events WHERE mm_node_id = 'bc-b1-fanout-0'",
    )
    .fetch_one(&pool)
    .await
    .expect("key");
    assert_eq!(key, format!("lease:bc-b1-fanout-0:{}:0", started.timestamp()));

    // Samples for the metered window are gone (V052's retention note).
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_relay_usage_samples")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(left, 0);

    // A replay at the same `now`: the meter clock advanced to the window's end, so
    // the sweep re-derives nothing — and even a clock reset would hit the
    // idempotency key. Nothing new is written either way.
    let s2 = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(s2.events_written, 0, "a replay must write nothing new");
    sqlx::query("UPDATE mm_fleet_nodes SET last_metered_at = NULL WHERE mm_node_id = 'bc-b1-fanout-0'")
        .execute(&pool)
        .await
        .expect("reset clock");
    let s3 = sweep_relay_hours(&pool, &meter, now).await;
    assert_eq!(
        s3.events_written, 0,
        "same (node, period) with a reset clock: the idempotency key de-duplicates"
    );
    let r2 = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r2.len(), 1, "no duplicate row from either replay");
});

// Review fix (Critical 2): a node created mid-period on an HOURLY provider bills
// windows anchored at billing_started_at — created 12:17, the first window is
// 12:17-13:17, not the wall-clock 12:00-13:00 hour.
pg_test!(a_mid_period_created_nodes_first_window_starts_at_billing_start, pool, {
    stream(&pool, "b1", "@host:hs").await;
    wallet(&pool, "@host:hs").await;
    let started = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
        + Duration::seconds(17 * 60); // :17 past an hour boundary region, unaligned
    fanout_node(&pool, "bc-b1-fanout-0", "hetzner", started).await;
    sample(&pool, "bc-b1-fanout-0", "b1", started + Duration::seconds(600), 4).await;

    let meter = PgMeteringDb::new(pool.clone());
    // 55 minutes in: the first (anchored) hour has not completed — nothing bills,
    // even though a wall-clock hour boundary has passed in between.
    let early = sweep_relay_hours(&pool, &meter, started + Duration::seconds(55 * 60)).await;
    assert_eq!(early.events_written, 0, "{:?}", early.errors);

    // 61 minutes in: the started..started+1h window is complete.
    let s = sweep_relay_hours(&pool, &meter, started + Duration::seconds(61 * 60)).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);
    let key: String = sqlx::query_scalar(
        "SELECT idempotency_key FROM mm_usage_events WHERE mm_node_id = 'bc-b1-fanout-0'",
    )
    .fetch_one(&pool)
    .await
    .expect("key");
    assert_eq!(
        key,
        format!("lease:bc-b1-fanout-0:{}:0", started.timestamp()),
        "period_start_unix is the anchored window start, not a wall-clock boundary"
    );
    let r = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r[0].1, 1000, "a full anchored hour");
});

// Review fix (Critical 1): a node that goes GONE mid-period with unswept samples
// still has its final partial window metered (to its normal payers) and those
// samples deleted — mirroring transcode_nodes_to_meter's gone catch-up clause.
// Without it, both the revenue and the samples would leak forever.
pg_test!(a_gone_nodes_final_partial_period_is_metered_and_its_samples_removed, pool, {
    stream(&pool, "b1", "@host:hs").await;
    wallet(&pool, "@host:hs").await;
    let started = DateTime::<Utc>::from_timestamp(1_700_000_013, 0).unwrap();
    fanout_node(&pool, "bc-b1-fanout-0", "scaleway", started).await;
    sample(&pool, "bc-b1-fanout-0", "b1", started + Duration::seconds(10), 5).await;
    // Gone 30 s into the first minute window: a partial window with samples in it.
    mark_gone(&pool, "bc-b1-fanout-0", started + Duration::seconds(30)).await;

    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_relay_hours(&pool, &meter, started + Duration::seconds(300)).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);

    let r = rows(&pool, "bc-b1-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].2, "wallet", "the final window bills its normal payer");
    // 30 s = 8.33 milli-hours, half-up to 8: the PARTIAL window, not a whole minute.
    assert_eq!(r[0].1, 8);

    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_relay_usage_samples")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(left, 0, "the gone node's samples were consumed, not leaked");

    // And the node never meters again: the clock sits at gone_at.
    let again = sweep_relay_hours(&pool, &meter, started + Duration::seconds(900)).await;
    assert_eq!(again.events_written, 0);
});

// A broadcast with no wallet is billed to the house, not dropped and not refused.
pg_test!(an_uncovered_broadcast_is_billed_to_the_house, pool, {
    stream(&pool, "b2", "@host2:hs").await; // no wallet row for @host2:hs
    let started = DateTime::<Utc>::from_timestamp(1_700_000_013, 0).unwrap();
    fanout_node(&pool, "bc-b2-fanout-0", "scaleway", started).await;
    sample(&pool, "bc-b2-fanout-0", "b2", started + Duration::seconds(10), 3).await;

    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_relay_hours(&pool, &meter, started + Duration::seconds(80)).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);
    let r = rows(&pool, "bc-b2-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].2, "house");
    assert_eq!(r[0].3, "b2", "an uncovered slice keeps its REAL broadcast id");
    assert_eq!(r[0].4, "house", "…under the fixed house user");
});

// A relay that billed with no samples at all for the window still gets metered —
// a single house slice for the whole window, never silently skipped.
pg_test!(a_quiet_relay_still_bills_the_house_for_the_period, pool, {
    let started = DateTime::<Utc>::from_timestamp(1_700_000_013, 0).unwrap();
    fanout_node(&pool, "bc-idle-fanout-0", "hetzner", started).await;

    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_relay_hours(&pool, &meter, started + Duration::seconds(3_900)).await;
    assert_eq!(s.events_written, 1, "{:?}", s.errors);
    let r = rows(&pool, "bc-idle-fanout-0").await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "relay_hour");
    assert_eq!(r[0].2, "house");
    assert_eq!(r[0].1, 1000, "hetzner is hour-quantized: the whole hour to the house");
    assert_eq!(r[0].3, "bc-idle-fanout-0", "no broadcast at all: the node id stands in");
    assert_eq!(r[0].4, "house");
});
