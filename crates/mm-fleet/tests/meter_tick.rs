//! The meter tick: poll, record, and only then charge (WS-D, §17.5).
//!
//! `sweep_egress` and `rate_pending` each have their own tests. What this file is
//! for is the seam between them — the part that decides whether money moves, and how
//! big the bill is when it starts moving.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use mm_core::switch_client::SwitchClient;
use mm_db::metering_db::PgMeteringDb;
use mm_db::wallet_db::PgWalletDb;
use mm_fleet::meter_loop::meter_tick;
use mm_fleet::metering::MeteredNode;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::net::TcpListener;
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

fn tick_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("maintenance door");
    for t in [
        "mm_egress_baseline",
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_rate_card",
        "mm_broadcaster_wallet",
        "mm_streams",
        "mm_rooms",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
    tx.commit().await.expect("commit wipe");
}

/// A broadcast with a host and a funded wallet, so its bytes have a payer.
async fn billable_broadcast(pool: &PgPool, broadcast: &str, host: &str, deposit: i64) {
    let matrix_room_id = format!("!room-{broadcast}:hs");
    sqlx::query(
        "INSERT INTO mm_rooms (matrix_room_id) VALUES ($1)
         ON CONFLICT (matrix_room_id) DO NOTHING",
    )
    .bind(&matrix_room_id)
    .execute(pool)
    .await
    .expect("room");
    let room_id: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(&matrix_room_id)
        .fetch_one(pool)
        .await
        .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status)
         VALUES ($1, $2, $3, 'active') ON CONFLICT (id) DO NOTHING",
    )
    .bind(broadcast)
    .bind(room_id)
    .bind(host)
    .execute(pool)
    .await
    .expect("stream");

    let wallet = PgWalletDb::new(pool.clone());
    wallet.create_wallet(host, "eur", 0).await.expect("wallet");
    if deposit > 0 {
        wallet
            .deposit(host, "eur", deposit, &format!("dep-{host}"))
            .await
            .expect("deposit");
    }
}

async fn rate_card(pool: &PgPool, price: i64) {
    sqlx::query(
        "INSERT INTO mm_rate_card (version, currency, unit, price_minor)
         VALUES (1, 'eur', 'egress_gb', $1) ON CONFLICT DO NOTHING",
    )
    .bind(price)
    .execute(pool)
    .await
    .expect("rate card");
}

type FakeState = (Arc<Mutex<Value>>, Arc<Mutex<bool>>);

struct FakeSwitch {
    reading: Arc<Mutex<Value>>,
    #[allow(dead_code)]
    fail: Arc<Mutex<bool>>,
}

async fn fake_switch(initial: Value) -> (String, FakeSwitch) {
    let reading = Arc::new(Mutex::new(initial));
    let fail = Arc::new(Mutex::new(false));
    let app = Router::new()
        .route(
            "/api/egress",
            get(|State((reading, fail)): State<FakeState>| async move {
                if *fail.lock().unwrap() {
                    return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                }
                Ok(Json(reading.lock().unwrap().clone()))
            }),
        )
        .with_state((reading.clone(), fail.clone()));
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), FakeSwitch { reading, fail })
}

fn reading(epoch: &str, pairs: &[(&str, i64)]) -> Value {
    json!({
        "epoch": epoch,
        "since": "2023-11-14T22:13:20Z",
        "overhead_bytes_per_packet": 38,
        "sources": pairs.iter().map(|(s, b)| json!({"source": s, "bytes": b})).collect::<Vec<_>>(),
    })
}

fn node(id: &str, base: &str) -> MeteredNode {
    MeteredNode {
        mm_node_id: id.into(),
        client: Arc::new(SwitchClient::new(base)),
    }
}

async fn balance(pool: &PgPool, user: &str) -> i64 {
    PgWalletDb::new(pool.clone())
        .get(user)
        .await
        .expect("get")
        .expect("wallet")
        .balance_minor
}

/// Two gigabytes, as the switch would report them (decimal GB — FR-304).
const TWO_GB: i64 = 2_000_000_000;

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = tick_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// ── Billing disabled ─────────────────────────────────────────────────────────

// THE DEFAULT. Metering must record usage and charge nobody, because deploying a
// release must not start taking money.
pg_test!(with_billing_off_usage_is_recorded_and_no_money_moves, pool, {
    billable_broadcast(&pool, "b1", "@host:hs", 100_000).await;
    rate_card(&pool, 9).await; // A card EXISTS, and is still not permission to charge.
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    // First poll sets the baseline.
    meter_tick(&meter, &wallet, &nodes, false, 500, t0).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB)]);
    let tick = meter_tick(&meter, &wallet, &nodes, false, 500, t0 + Duration::seconds(60)).await;

    assert_eq!(tick.sweep.events_written, 1, "the usage must be recorded");
    assert_eq!(tick.sweep.metered_bytes, vec![("origin".to_string(), TWO_GB)]);
    assert!(
        tick.rating.is_none(),
        "no rating pass at all — which is not the same as a pass that charged nothing"
    );
    assert_eq!(balance(&pool, "@host:hs").await, 100_000, "no money moved");
    assert_eq!(
        tick.unrated_backlog,
        Some(1),
        "and the bill that enabling billing would send must be countable"
    );
});

// The backlog is the thing an operator needs before flipping the switch, so it has
// to grow visibly rather than being discovered from the first charge.
pg_test!(the_backlog_grows_while_billing_is_off, pool, {
    billable_broadcast(&pool, "b1", "@host:hs", 100_000).await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();
    meter_tick(&meter, &wallet, &nodes, false, 500, t0).await;

    let mut seen = Vec::new();
    for i in 1..=3 {
        *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB * i)]);
        let tick =
            meter_tick(&meter, &wallet, &nodes, false, 500, t0 + Duration::seconds(60 * i)).await;
        seen.push(tick.unrated_backlog);
    }
    assert_eq!(seen, vec![Some(1), Some(2), Some(3)]);
});

// ── Turning billing on ───────────────────────────────────────────────────────

// ⚠️ The documented consequence of metering early and charging later: the first
// billed tick charges EVERYTHING accumulated. Asserted rather than merely commented,
// because this is the behaviour that surprises an operator, and a test is the only
// place it cannot be quietly changed.
pg_test!(the_first_billed_tick_charges_the_whole_backlog, pool, {
    billable_broadcast(&pool, "b1", "@host:hs", 100_000).await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    meter_tick(&meter, &wallet, &nodes, false, 500, t0).await;
    for i in 1..=3 {
        *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB * i)]);
        meter_tick(&meter, &wallet, &nodes, false, 500, t0 + Duration::seconds(60 * i)).await;
    }
    assert_eq!(balance(&pool, "@host:hs").await, 100_000, "still untouched");

    // The operator sets a price and switches billing on.
    rate_card(&pool, 9).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB * 4)]);
    let tick = meter_tick(&meter, &wallet, &nodes, true, 500, t0 + Duration::seconds(240)).await;

    let r = tick.rating.expect("billing on means a rating pass ran");
    assert_eq!(r.rated, 4, "three backlogged intervals plus this tick's");
    // 2 GB at 9 per GB = 18, four times.
    assert_eq!(r.charged_minor, 72);
    assert_eq!(balance(&pool, "@host:hs").await, 100_000 - 72);
    assert_eq!(tick.unrated_backlog, Some(0), "the queue must be empty after");
});

// ── The order of the two halves ──────────────────────────────────────────────

// Sweep THEN rate, so this tick's own usage is charged this tick. The other order
// delays every charge by one interval for no benefit, which on a node torn down
// immediately after is a charge that never happens at all.
pg_test!(usage_recorded_this_tick_is_charged_this_tick, pool, {
    billable_broadcast(&pool, "b1", "@host:hs", 100_000).await;
    rate_card(&pool, 9).await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    meter_tick(&meter, &wallet, &nodes, true, 500, t0).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB)]);
    let tick = meter_tick(&meter, &wallet, &nodes, true, 500, t0 + Duration::seconds(60)).await;

    assert_eq!(tick.sweep.events_written, 1);
    let r = tick.rating.expect("rating ran");
    assert_eq!(r.rated, 1, "the very usage this tick recorded");
    assert_eq!(r.charged_minor, 18);
    assert_eq!(tick.unrated_backlog, Some(0));
});

// ── Failure isolation across the seam ────────────────────────────────────────

// An empty wallet must not stop metering: the bytes are real and the debt is real,
// and a meter that stops when a wallet empties loses the record of what was owed.
pg_test!(an_empty_wallet_stops_the_charge_but_not_the_meter, pool, {
    billable_broadcast(&pool, "b1", "@broke:hs", 5).await;
    rate_card(&pool, 9).await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    meter_tick(&meter, &wallet, &nodes, true, 500, t0).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB)]);
    let tick = meter_tick(&meter, &wallet, &nodes, true, 500, t0 + Duration::seconds(60)).await;

    assert_eq!(tick.sweep.events_written, 1, "the usage is still recorded");
    let r = tick.rating.expect("rating ran");
    assert_eq!(r.rated, 0);
    assert_eq!(r.insufficient_funds, vec!["b1"]);
    assert_eq!(balance(&pool, "@broke:hs").await, 5);
    assert_eq!(
        tick.unrated_backlog,
        Some(1),
        "the debt stays on the queue, to rate when the wallet is topped up"
    );
});

// One unreachable node must not stop the other being metered OR the queue being
// charged — a single bad node costing the fleet's revenue is FR-305f's whole point.
pg_test!(one_unreachable_node_costs_only_its_own_revenue, pool, {
    billable_broadcast(&pool, "b1", "@a:hs", 100_000).await;
    billable_broadcast(&pool, "b2", "@b:hs", 100_000).await;
    rate_card(&pool, 9).await;
    let (base_a, sw_a) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let (base_b, sw_b) = fake_switch(reading("e1", &[("stream-b2", 0)])).await;
    // ORDER MATTERS, and it is the whole test. With the healthy node first, an
    // implementation that aborts the sweep on the first failure passes every
    // assertion below — it had already polled node-a by the time node-b failed. The
    // failing node goes first so that "node-b was metered" can only be true if the
    // failure was isolated.
    let nodes = vec![node("node-a", &base_a), node("node-b", &base_b)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    meter_tick(&meter, &wallet, &nodes, true, 500, t0).await;
    *sw_a.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB)]);
    *sw_b.reading.lock().unwrap() = reading("e1", &[("stream-b2", TWO_GB)]);
    // node-a is the one that breaks, and it is first in the list.
    *sw_a.fail.lock().unwrap() = true;

    let tick = meter_tick(&meter, &wallet, &nodes, true, 500, t0 + Duration::seconds(60)).await;

    assert_eq!(
        tick.sweep.polled,
        vec!["node-b"],
        "the node AFTER the failure must still be metered"
    );
    assert_eq!(tick.sweep.unreachable.len(), 1);
    assert_eq!(tick.sweep.unreachable[0].0, "node-a");
    let r = tick.rating.expect("rating ran");
    assert_eq!(r.rated, 1, "node-b's usage is charged regardless");
    assert_eq!(balance(&pool, "@b:hs").await, 100_000 - 18);
    assert_eq!(balance(&pool, "@a:hs").await, 100_000, "nothing metered, nothing charged");
});

// ── Metered bytes are for calibration, so they must not double-count ──────────

// A second poll of an unchanged counter must add nothing — no usage and no bytes.
// This is the no-delta path, not the de-duplication path: the baseline advanced with
// the first write, so there is nothing to re-derive. The UNIQUE-constraint path is
// tested at `record_interval` itself, in mm-db, which is where that guard lives.
pg_test!(a_replayed_tick_adds_no_metered_bytes, pool, {
    billable_broadcast(&pool, "b1", "@host:hs", 100_000).await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 0)])).await;
    let nodes = vec![node("origin", &base)];
    let meter = PgMeteringDb::new(pool.clone());
    let wallet = PgWalletDb::new(pool.clone());
    let t0 = Utc::now();

    meter_tick(&meter, &wallet, &nodes, false, 500, t0).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", TWO_GB)]);
    let first = meter_tick(&meter, &wallet, &nodes, false, 500, t0 + Duration::seconds(60)).await;
    assert_eq!(first.sweep.metered_bytes, vec![("origin".to_string(), TWO_GB)]);

    // Same reading again: the baseline has advanced, so there is no delta at all.
    let second = meter_tick(&meter, &wallet, &nodes, false, 500, t0 + Duration::seconds(120)).await;
    assert_eq!(second.sweep.events_written, 0);
    assert!(
        second.sweep.metered_bytes.is_empty(),
        "a tick that wrote nothing must report no bytes: {:?}",
        second.sweep.metered_bytes
    );
    assert_eq!(second.unrated_backlog, Some(1), "still the one event");
});
