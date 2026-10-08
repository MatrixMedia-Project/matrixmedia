//! The egress meter end to end: a stand-in mm-switch, a real database (WS-D, FR-302a/b).
//!
//! The pure delta is unit-tested in the module. What needs both a server and a
//! database is the loop's *durability* behaviour — that a baseline survives a poll,
//! that a replayed poll bills nothing twice, that a node restart does not silently
//! swallow an interval, and that one unreachable node does not cost the others their
//! revenue.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use mm_core::switch_client::SwitchClient;
use mm_db::metering_db::PgMeteringDb;
use mm_fleet::metering::{sweep_egress, MeteredNode};
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

fn meter_lock() -> &'static AsyncMutex<()> {
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
        // Streams before rooms: mm_streams.room_id references mm_rooms(id).
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
async fn billable_broadcast(pool: &PgPool, broadcast: &str, host: &str) {
    // The column is `matrix_room_id`, read from V007 rather than guessed — the first
    // draft of this fixture invented `room_id` and every test failed on the fixture
    // instead of on the thing under test.
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

    sqlx::query(
        "INSERT INTO mm_broadcaster_wallet (user_id, currency) VALUES ($1, 'eur')
         ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(host)
    .execute(pool)
    .await
    .expect("wallet");
}

/// A stand-in mm-switch whose egress reading the test controls.
struct FakeSwitch {
    reading: Arc<Mutex<Value>>,
    fail: Arc<Mutex<bool>>,
}

/// Named so the handler signature stays readable — an inline
/// `State<(Arc<Mutex<Value>>, Arc<Mutex<bool>>)>` is the kind of type that makes a
/// test harder to read than the thing it tests.
type FakeState = (Arc<Mutex<Value>>, Arc<Mutex<bool>>);

async fn fake_switch(initial: Value) -> (String, FakeSwitch) {
    let reading = Arc::new(Mutex::new(initial));
    let fail = Arc::new(Mutex::new(false));
    let state = (reading.clone(), fail.clone());

    let app = Router::new().route(
        "/api/egress",
        get(
            |State((reading, fail)): State<FakeState>| async move {
                if *fail.lock().unwrap() {
                    return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
                }
                Ok(Json(reading.lock().unwrap().clone()))
            },
        ),
    )
    .with_state(state);

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

async fn usage_rows(pool: &PgPool) -> Vec<(String, String, i64, i64)> {
    sqlx::query_as(
        "SELECT broadcast_id, user_id, quantity_milli, cumulative_bytes
           FROM mm_usage_events WHERE unit = 'egress_gb' ORDER BY cumulative_bytes",
    )
    .fetch_all(pool)
    .await
    .expect("usage rows")
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = meter_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// ── The baseline, which is the whole point of persisting anything ─────────────

// The first poll must bill nothing and store a baseline. Billing a first cumulative
// reading would charge for everything since the node booted.
pg_test!(the_first_poll_stores_a_baseline_and_bills_nothing, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, _sw) = fake_switch(reading("e1", &[("stream-b1", 5_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    assert_eq!(sweep.polled, vec!["n1"]);
    assert_eq!(sweep.events_written, 0, "a first reading must not be billed");
    assert!(usage_rows(&pool).await.is_empty());

    let stored = db.baselines_for_node("n1").await.expect("baselines");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].cumulative_bytes, 5_000_000_000);
    assert_eq!(stored[0].epoch, "e1");
});

// The second poll bills the difference, and the baseline moves with it.
pg_test!(the_second_poll_bills_the_difference, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", 3_500_000_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    assert_eq!(sweep.events_written, 1);
    let rows = usage_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    let (broadcast, user, qty, cumulative) = &rows[0];
    assert_eq!(broadcast, "b1");
    assert_eq!(user, "@host:hs");
    assert_eq!(*qty, 2_500, "2.5 GB delivered = 2500 milli-GB");
    assert_eq!(*cumulative, 3_500_000_000, "traceable to the counter reading");

    let stored = db.baselines_for_node("n1").await.expect("baselines");
    assert_eq!(stored[0].cumulative_bytes, 3_500_000_000);
});

// THE RETRY PROPERTY. Polling the same reading twice must bill once. The key derives
// from the cumulative value, so a replay collides at V035's UNIQUE constraint and is
// a no-op — which is what makes the meter safe to retry rather than merely retried.
pg_test!(polling_the_same_reading_twice_bills_once, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", 2_000_000_000)]);

    let first = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(first.events_written, 1);

    // The same reading again — as would happen if the write committed but the caller
    // crashed before recording that it had polled.
    let second = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(
        second.events_written, 0,
        "the replay must be a no-op, not a second charge"
    );
    assert_eq!(usage_rows(&pool).await.len(), 1);
});

// ── A node restart, which is how an ephemeral node's life ends ────────────────

// A naive delta here is negative, and code that clamps it to zero bills NOTHING for
// the node's entire second life. The epoch makes it billable and the loss bounded.
pg_test!(a_node_restart_bills_the_new_epoch_and_reports_the_gap, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 9_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    // Restart: new epoch, counters from zero.
    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 1_200_000_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    assert_eq!(sweep.events_written, 1);
    let rows = usage_rows(&pool).await;
    assert_eq!(
        rows[0].2, 1_200,
        "the whole post-restart reading is new usage — clamping a negative delta to \
         zero would have billed none of it"
    );
    assert_eq!(sweep.anomalies.len(), 1, "and the lost gap must be reported");

    let stored = db.baselines_for_node("n1").await.expect("baselines");
    assert_eq!(stored[0].epoch, "e2", "the baseline follows the new epoch");
});

// ── Failure isolation ────────────────────────────────────────────────────────

// One unreachable node must not cost the others their revenue. Aborting the sweep on
// the first failure would mean a single bad node unbills the whole fleet.
pg_test!(one_unreachable_node_does_not_stop_the_others, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    billable_broadcast(&pool, "b2", "@host2:hs").await;
    let (good, sw_good) = fake_switch(reading("e1", &[("stream-b1", 1_000_000_000)])).await;
    let (bad, sw_bad) = fake_switch(reading("e1", &[("stream-b2", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    let nodes = vec![node("bad", &bad), node("good", &good)];
    sweep_egress(&db, &nodes, Utc::now()).await;

    *sw_good.reading.lock().unwrap() = reading("e1", &[("stream-b1", 3_000_000_000)]);
    *sw_bad.fail.lock().unwrap() = true;

    let sweep = sweep_egress(&db, &nodes, Utc::now()).await;

    assert_eq!(sweep.unreachable.len(), 1);
    assert_eq!(sweep.unreachable[0].0, "bad");
    assert_eq!(sweep.polled, vec!["good"]);
    assert_eq!(sweep.events_written, 1, "the healthy node must still be billed");
});

// A source with no payer is reported, not billed and not dropped in silence. Bytes
// with nobody to charge are a product problem, and one that hides is worse.
pg_test!(bytes_with_no_payer_are_reported, pool, {
    // No broadcast row and no wallet for this source.
    let (base, sw) = fake_switch(reading("e1", &[("stream-orphan", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-orphan", 4_000_000_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    assert_eq!(sweep.unbillable, vec!["stream-orphan"]);
    assert_eq!(sweep.events_written, 0);
    assert!(usage_rows(&pool).await.is_empty());
});

// The baseline still advances past bytes nobody can pay for. Holding it back makes
// every later poll re-derive the same unbillable delta, so the source is reported on
// every tick and its `observed_at` never moves — on steegler (2026-10-06 → 10-08) the
// origin's only row stayed frozen for two days while broadcasts ran.
pg_test!(bytes_with_no_payer_still_advance_the_baseline, pool, {
    let (base, sw) = fake_switch(reading("e1", &[("stream-orphan", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    let t0 = Utc::now() - Duration::minutes(10);
    sweep_egress(&db, &[node("n1", &base)], t0).await;

    *sw.reading.lock().unwrap() = reading("e1", &[("stream-orphan", 4_000_000_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(sweep.unbillable, vec!["stream-orphan"]);

    let stored = db.baselines_for_node("n1").await.expect("baselines");
    assert_eq!(
        stored[0].cumulative_bytes, 4_000_000_000,
        "a source with no payer must still have its baseline advanced"
    );
    assert!(stored[0].observed_at > t0, "and its observation time with it");

    // Same reading again: those bytes were reported on the poll that saw them.
    let again = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert!(
        again.unbillable.is_empty(),
        "bytes already reported must not be reported again on every tick"
    );
});

// ── A restart must leave the node on ONE epoch ────────────────────────────────

// A source the old process served and the new one has not is absent from the new
// reading. If its row stays on the old epoch the node's baseline is mixed, the sweep
// discards it as untrustworthy, and every later poll is a "first reading" that bills
// nothing — billing on that node stops, silently.
pg_test!(a_source_missing_after_a_restart_does_not_pin_the_old_epoch, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    billable_broadcast(&pool, "b2", "@host2:hs").await;
    let (base, sw) = fake_switch(reading(
        "e1",
        &[("stream-b1", 2_000_000_000), ("stream-b2", 1_000_000_000)],
    ))
    .await;
    let db = PgMeteringDb::new(pool.clone());
    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    // Restart. b2 ended before it, so the new process never served it.
    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 1_500_000_000)]);
    let restart = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(restart.events_written, 1);
    assert_eq!(restart.anomalies.len(), 1, "the restart itself is reported");

    let stored = db.baselines_for_node("n1").await.expect("baselines");
    assert!(
        stored.iter().all(|r| r.epoch == "e2"),
        "every stored row must follow the node to its new epoch, got {stored:?}"
    );

    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 2_500_000_000)]);
    let next = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(
        next.events_written, 1,
        "the poll after a restart must subtract, not start over as a first reading"
    );
    assert!(next.anomalies.is_empty(), "{:?}", next.anomalies);
    let rows = usage_rows(&pool).await;
    assert_eq!(rows.last().map(|r| r.2), Some(1_000), "2.5 GB - 1.5 GB");
});

// A restart nobody is watching yet: the new reading has no sources at all. It must be
// reported once, not on every tick until somebody broadcasts — and the first source to
// appear afterwards started from zero in this epoch, so all of it is billable.
pg_test!(a_restart_into_an_empty_reading_is_reported_once, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());
    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    *sw.reading.lock().unwrap() = reading("e2", &[]);
    let restart = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(restart.anomalies.len(), 1);

    let quiet = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert!(
        quiet.anomalies.is_empty(),
        "one restart is one anomaly, not one per tick: {:?}",
        quiet.anomalies
    );

    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 2_000_000_000)]);
    let live = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(live.events_written, 1);
    assert!(live.anomalies.is_empty(), "{:?}", live.anomalies);
    assert_eq!(usage_rows(&pool).await.last().map(|r| r.2), Some(2_000));
});

// The steegler shape (2026-10-08): no broadcaster has a wallet, the switch restarted,
// and every source in the new epoch has bytes with no payer. Nothing was written, so
// each one-minute tick re-reported the same CountersReset with the same two epochs.
pg_test!(a_restart_where_nobody_pays_is_reported_once, pool, {
    let (base, sw) = fake_switch(reading("e1", &[("stream-x", 12_685_588)])).await;
    let db = PgMeteringDb::new(pool.clone());
    sweep_egress(&db, &[node("origin", &base)], Utc::now()).await;

    *sw.reading.lock().unwrap() =
        reading("e2", &[("stream-y", 5_000_000), ("stream-z", 3_000_000)]);
    let restart = sweep_egress(&db, &[node("origin", &base)], Utc::now()).await;
    assert_eq!(restart.anomalies.len(), 1);
    assert_eq!(restart.unbillable, vec!["stream-y", "stream-z"]);

    let next = sweep_egress(&db, &[node("origin", &base)], Utc::now()).await;
    assert!(
        next.anomalies.is_empty(),
        "the same restart must not be reported again: {:?}",
        next.anomalies
    );
    assert!(next.unbillable.is_empty(), "{:?}", next.unbillable);
    let stored = db.baselines_for_node("origin").await.expect("baselines");
    assert!(stored.iter().all(|r| r.epoch == "e2"), "{stored:?}");
});

// ── Sub-megabyte deltas accumulate rather than being discarded ────────────────

// A delta under a megabyte rounds to zero milli-GB. Advancing the baseline past it
// would throw those bytes away a fraction at a time — and at poll frequency that is
// a systematic under-bill. The baseline is deliberately NOT advanced, so they
// accumulate into the next interval that does cross a megabyte.
pg_test!(a_sub_megabyte_delta_accumulates_instead_of_vanishing, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 1_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    let after_first = db.baselines_for_node("n1").await.expect("b")[0].cumulative_bytes;
    assert_eq!(after_first, 1_000_000);

    // +400 KB: rounds to 0 milli-GB.
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", 1_400_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(sweep.events_written, 0);
    assert_eq!(
        db.baselines_for_node("n1").await.expect("b")[0].cumulative_bytes,
        1_000_000,
        "the baseline must NOT advance past unbilled bytes, or they are lost a \
         fraction at a time on every poll"
    );

    // +1.2 MB total since the baseline: now it crosses.
    *sw.reading.lock().unwrap() = reading("e1", &[("stream-b1", 2_200_000)]);
    let sweep = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(sweep.events_written, 1);
    let rows = usage_rows(&pool).await;
    assert_eq!(
        rows[0].2, 1,
        "1.2 MB accumulated across two intervals bills as 1 milli-GB, not 0"
    );
});

// A sub-megabyte delta in a NEW epoch is held back like any other — but its row must
// not hold the node on the old epoch while it waits, or the restart is reported on
// every tick until the source crosses a megabyte.
pg_test!(a_sub_megabyte_source_after_a_restart_is_billed_from_zero, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, sw) = fake_switch(reading("e1", &[("stream-b1", 2_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());
    sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;

    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 400_000)]);
    let restart = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert_eq!(restart.events_written, 0, "400 KB is under a megabyte");
    assert_eq!(restart.anomalies.len(), 1);

    *sw.reading.lock().unwrap() = reading("e2", &[("stream-b1", 1_400_000)]);
    let next = sweep_egress(&db, &[node("n1", &base)], Utc::now()).await;
    assert!(next.anomalies.is_empty(), "{:?}", next.anomalies);
    assert_eq!(next.events_written, 1);
    assert_eq!(
        usage_rows(&pool).await.last().map(|r| r.2),
        Some(1),
        "1.4 MB since the restart bills as 1 milli-GB"
    );
});

// ── A quiet source still advances, so a stalled meter is visible ──────────────

// A source that delivers nothing must still have its `observed_at` advance, or it is
// indistinguishable from a meter that has stopped reporting — and a stalled meter is
// unbilled revenue.
pg_test!(a_quiet_source_still_advances_its_observation_time, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, _sw) = fake_switch(reading("e1", &[("stream-b1", 1_000_000_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    let t0 = Utc::now() - Duration::minutes(10);
    sweep_egress(&db, &[node("n1", &base)], t0).await;
    let first = db.baselines_for_node("n1").await.expect("b")[0].observed_at;

    // Same reading, later poll: no usage, but the observation must move.
    let t1 = Utc::now();
    sweep_egress(&db, &[node("n1", &base)], t1).await;
    let second = db.baselines_for_node("n1").await.expect("b")[0].observed_at;

    assert!(
        second > first,
        "a quiet source whose observed_at never moves looks exactly like a meter that \
         has stopped, and the difference is revenue"
    );
});

// And the query that finds one.
pg_test!(a_stalled_meter_is_findable, pool, {
    billable_broadcast(&pool, "b1", "@host:hs").await;
    let (base, _sw) = fake_switch(reading("e1", &[("stream-b1", 1_000)])).await;
    let db = PgMeteringDb::new(pool.clone());

    sweep_egress(&db, &[node("n1", &base)], Utc::now() - Duration::hours(2)).await;

    let stale = db
        .stale_meters(Utc::now() - Duration::hours(1))
        .await
        .expect("stale query");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].0, "n1");

    let fresh = db
        .stale_meters(Utc::now() - Duration::hours(3))
        .await
        .expect("stale query");
    assert!(fresh.is_empty(), "a meter within the cutoff is not stale");
});
