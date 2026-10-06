//! The ladder's quote against a real database (WS-D, §17.4).
//!
//! The projection arithmetic is unit-tested in the module; this is for the SQL — the
//! egress window, its clipping to the broadcast's age, the node count and the owed
//! backlog — which mixes a float bind with `EXTRACT(EPOCH …)`, the kind of type
//! resolution that only a real Postgres settles.

use std::sync::OnceLock;

use mm_fleet::ladder_billing::LadderBillingSource;
use mm_fleet::runner::BillingSource;
use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn setup(pool: &PgPool) {
    static M: OnceLock<Mutex<bool>> = OnceLock::new();
    let mut done = M.get_or_init(|| Mutex::new(false)).lock().await;
    if !*done {
        mm_db::run_pg_migrations(pool).await.expect("migrate");
        *done = true;
    }
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'").execute(&mut *tx).await.expect("door");
    for t in [
        "mm_fleet_nodes", "mm_usage_events", "mm_wallet_transactions", "mm_rate_card",
        "mm_broadcaster_wallet", "mm_streams", "mm_rooms",
    ] {
        sqlx::query(&format!("DELETE FROM {t}")).execute(&mut *tx).await.expect("wipe");
    }
    tx.commit().await.expect("commit");
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// A live broadcast that started `age_secs` ago, whose host has `deposit` in a wallet.
async fn broadcast(pool: &PgPool, id: &str, host: &str, age_secs: i64, deposit: i64) {
    let r = format!("!r-{id}:hs");
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1)").bind(&r).execute(pool).await.expect("room");
    let room: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(&r).fetch_one(pool).await.expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status, started_at)
         VALUES ($1, $2, $3, 'active', now() - make_interval(secs => $4))",
    )
    .bind(id).bind(room).bind(host).bind(age_secs as f64)
    .execute(pool).await.expect("stream");
    let w = mm_db::wallet_db::PgWalletDb::new(pool.clone());
    w.create_wallet(host, "eur", 0).await.expect("wallet");
    if deposit > 0 {
        w.deposit(host, "eur", deposit, &format!("dep-{host}")).await.expect("deposit");
    }
}

async fn price(pool: &PgPool, unit: &str, p: i64) {
    sqlx::query("INSERT INTO mm_rate_card (version, currency, unit, price_minor) VALUES (1, 'eur', $1, $2)")
        .bind(unit).bind(p).execute(pool).await.expect("price");
}

async fn egress(pool: &PgPool, user: &str, broadcast: &str, milli: i64, secs_ago: i64, key: &str) {
    sqlx::query(
        "INSERT INTO mm_usage_events (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at)
         VALUES ($1, $2, 'egress_gb', $3, $4, now() - make_interval(secs => $5))",
    )
    .bind(user).bind(broadcast).bind(milli).bind(key).bind(secs_ago as f64)
    .execute(pool).await.expect("usage");
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _g = lock().lock().await;
            setup(&$pool).await;
            $body
        }
    };
}

// The case the planner's quote got wrong: origin-only, egress-only card. The cost is
// the measured egress rate over the horizon, and nothing for nodes that do not exist.
pg_test!(an_origin_only_broadcast_is_quoted_on_its_egress, pool, {
    broadcast(&pool, "b1", "@h:hs", 3_600, 10_000).await;
    price(&pool, "egress_gb", 9).await; // no node_minute: legitimate for the ladder
    egress(&pool, "@h:hs", "b1", 1_000, 300, "e1").await; // 1 GB five minutes ago

    let q = LadderBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");

    // 1 GB per 10-minute window = 6 GB/hour at 9 = 54.
    assert_eq!(q.projected_cost_minor, 54);
    // And that 1 GB is metered, unrated, and owed from the same balance: 9.
    assert_eq!(q.available_balance_minor, 10_000 - 9);
});

// Egress older than the window is history, not rate.
pg_test!(egress_outside_the_window_is_not_part_of_the_rate, pool, {
    broadcast(&pool, "b1", "@h:hs", 7_200, 10_000).await;
    price(&pool, "egress_gb", 9).await;
    egress(&pool, "@h:hs", "b1", 5_000, 3_000, "old").await; // 50 minutes ago

    let q = LadderBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");
    assert_eq!(q.projected_cost_minor, 0, "nothing delivered recently");
    assert_eq!(q.available_balance_minor, 10_000 - 45, "but it is still owed");
});

// A two-minute-old broadcast's window is two minutes, not ten: 0.2 GB in its first
// two minutes is 6 GB/hour, not 1.2.
pg_test!(a_young_broadcasts_window_is_its_age, pool, {
    broadcast(&pool, "b1", "@h:hs", 120, 10_000).await;
    price(&pool, "egress_gb", 9).await;
    egress(&pool, "@h:hs", "b1", 200, 30, "e1").await;

    let q = LadderBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");
    assert_eq!(q.projected_cost_minor, 54);
});

// A running node with no node price is unpriceable — which the ladder SKIPS rather
// than reading as free.
pg_test!(a_running_node_without_a_node_price_is_unpriceable, pool, {
    broadcast(&pool, "b1", "@h:hs", 3_600, 10_000).await;
    price(&pool, "egress_gb", 9).await;
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline)
         VALUES ('bc-b1-fanout-0', 'fanout', 'rented', 'scaleway', 'healthy', now() + interval '1 hour')",
    )
    .execute(&pool)
    .await
    .expect("node");

    let err = LadderBillingSource::new(pool.clone(), "eur").quote("b1").await.expect_err("unpriceable");
    assert!(err.contains("node_minute"), "{err}");
});

pg_test!(running_nodes_are_counted_and_no_more, pool, {
    broadcast(&pool, "b1", "@h:hs", 3_600, 100_000).await;
    price(&pool, "egress_gb", 9).await;
    price(&pool, "node_minute", 10).await;
    for i in 0..2 {
        sqlx::query(
            "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline)
             VALUES ($1, 'fanout', 'rented', 'scaleway', 'healthy', now() + interval '1 hour')",
        )
        .bind(format!("bc-b1-fanout-{i}"))
        .execute(&pool)
        .await
        .expect("node");
    }
    // A node already gone does not count.
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline)
         VALUES ('bc-b1-fanout-9', 'fanout', 'rented', 'scaleway', 'gone', now() + interval '1 hour')",
    )
    .execute(&pool)
    .await
    .expect("gone node");

    let q = LadderBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");
    assert_eq!(q.projected_cost_minor, 2 * 60 * 10, "two running node-hours, no phantom, no gone");
});

// FR-314: the planner's quote prices the GPU it may be about to order — one
// transcoder over the horizon — so the balance gate can judge the projection WITH
// it. An hour at 4 per GPU-minute is 240. A card without `gpu_minute` quotes 0,
// which the planner reads as "may not order one".
pg_test!(the_planner_quote_prices_one_more_transcoder, pool, {
    use mm_fleet::wallet_billing::WalletBillingSource;

    broadcast(&pool, "b1", "@h:hs", 60, 10_000).await;
    price(&pool, "node_minute", 1).await;

    let q = WalletBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");
    assert_eq!(q.transcoder_cost_minor, 0, "no gpu_minute price: the GPU is unpriced");
    assert_eq!(q.projected_cost_minor, 60, "one fan-out hour, no transcoder");
    assert!(q.broadcaster_is_paying);

    price(&pool, "gpu_minute", 4).await;
    let q = WalletBillingSource::new(pool.clone(), "eur").quote("b1").await.expect("quote");
    assert_eq!(q.transcoder_cost_minor, 240);
    assert_eq!(q.projected_cost_minor, 60, "the prospective GPU is NOT in the projection");
});
