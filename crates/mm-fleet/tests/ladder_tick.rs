//! The demotion ladder acting on real broadcasts (WS-D, design §17.4).
//!
//! The rung arithmetic and the hysteresis are unit-tested in `mm-core`; the mode
//! matrix is unit-tested in the module. What needs a database is the part that
//! decides whether a real broadcaster's stream is degraded or ended, and whether the
//! statement of reasons CR-604 requires actually exists when it is.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use mm_core::fleet::ladder::{DemotionStep, LadderPolicy};
use mm_db::ladder_db::PgLadderDb;
use mm_fleet::ladder_loop::{ladder_tick, LadderActuator, LadderMode, RecordingActuator};
use mm_fleet::runner::{BillingSource, BroadcastBilling};
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

fn ladder_lock() -> &'static AsyncMutex<()> {
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
        "mm_demotion_events",
        "mm_broadcast_demotion",
        "mm_egress_baseline",
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_broadcaster_wallet",
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

async fn live_broadcast(pool: &PgPool, broadcast: &str, host: &str) {
    let matrix_room_id = format!("!room-{broadcast}:hs");
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
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
}

/// A billing source the test drives directly: coverage is the only input the ladder
/// has, so the test sets it rather than building a wallet and a rate card to imply it.
/// broadcast -> (balance, projected), or the error its quote returns.
type Quotes = std::collections::HashMap<String, Result<(i64, i64), String>>;

#[derive(Default)]
struct FakeBilling {
    quotes: Mutex<Quotes>,
}

impl FakeBilling {
    fn set(&self, broadcast: &str, balance: i64, projected: i64) {
        self.quotes
            .lock()
            .unwrap()
            .insert(broadcast.into(), Ok((balance, projected)));
    }
    fn fail(&self, broadcast: &str, why: &str) {
        self.quotes
            .lock()
            .unwrap()
            .insert(broadcast.into(), Err(why.into()));
    }
}

#[async_trait]
impl BillingSource for FakeBilling {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String> {
        match self.quotes.lock().unwrap().get(broadcast_id) {
            Some(Ok((balance, projected))) => Ok(BroadcastBilling {
                available_balance_minor: *balance,
                projected_cost_minor: *projected,
                transcode_enabled: false,
            }),
            Some(Err(e)) => Err(e.clone()),
            None => Err("no quote configured".into()),
        }
    }
}

/// An actuator that refuses, to prove a failed actuation is recorded as failed.
struct BrokenActuator;

#[async_trait]
impl LadderActuator for BrokenActuator {
    async fn stop_recording(&self, _: &str) -> Result<(), String> {
        Err("the recorder is unreachable".into())
    }
    async fn drain_to_origin(&self, _: &str) -> Result<(), String> {
        Err("the switch is unreachable".into())
    }
    async fn end_with_slate(&self, _: &str, _: &str) -> Result<(), String> {
        Err("the homeserver is unreachable".into())
    }
}

/// `(target, applied, streak)`.
async fn rung(pool: &PgPool, broadcast: &str) -> Option<(String, String, i32)> {
    sqlx::query_as(
        "SELECT target_step, applied_step, milder_streak FROM mm_broadcast_demotion
          WHERE broadcast_id = $1",
    )
    .bind(broadcast)
    .fetch_optional(pool)
    .await
    .expect("rung")
}

/// `(kind, from, to, statement)`, oldest first.
async fn events(pool: &PgPool, broadcast: &str) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT kind, from_step, to_step, statement FROM mm_demotion_events
          WHERE broadcast_id = $1 ORDER BY id",
    )
    .bind(broadcast)
    .fetch_all(pool)
    .await
    .expect("events")
}

/// Only the rows that are statements of reasons to the broadcaster (CR-604).
async fn statements(pool: &PgPool, broadcast: &str) -> Vec<(String, String, String, String)> {
    events(pool, broadcast)
        .await
        .into_iter()
        .filter(|e| e.0 == "applied")
        .collect()
}

/// Recovery is immediate, so a test that is not about hysteresis is not about
/// hysteresis.
fn instant() -> LadderPolicy {
    LadderPolicy {
        recover_after_evaluations: 0,
        ..LadderPolicy::placeholder()
    }
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!("MM_DATABASE_URL not set — skipping ", stringify!($name)));
                return;
            };
            let _guard = ladder_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

// ── The default mode ─────────────────────────────────────────────────────────

// 🔴 THE ONE THAT MATTERS. With no rate card and no funded wallets, every broadcast
// computes a zero balance, which is `EndWithSlate`. In the default mode that must
// record a forecast and apply nothing.
pg_test!(observe_mode_records_the_forecast_and_applies_nothing, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Observe, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert!(
        actuator.calls.lock().unwrap().is_empty(),
        "the DEFAULT mode called {:?} — on a platform with no rate card that is every \
         live broadcast being terminated",
        actuator.calls.lock().unwrap()
    );
    assert_eq!(r.moved.len(), 1);
    assert!(r.applied.is_empty());
    let (target, applied, _) = rung(&pool, "b1").await.expect("a rung was recorded");
    assert_eq!(target, "end_with_slate", "the forecast is recorded");
    assert_eq!(applied, "healthy", "and nothing was done");
    let ev = events(&pool, "b1").await;
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, "decision", "a forecast is not a statement to the broadcaster");
    assert!(statements(&pool, "b1").await.is_empty());
});

// ── Degrade ──────────────────────────────────────────────────────────────────

pg_test!(degrade_mode_reduces_quality_without_ending_anything, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 400, 1_000); // coverage 0.4 -> ReduceQuality
    let actuator = RecordingActuator::default();

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(*actuator.calls.lock().unwrap(), vec!["stop_recording:b1".to_string()]);
    let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!((target.as_str(), applied.as_str()), ("reduce_quality", "reduce_quality"));
});

// REGRESSION (review 2026-09-25, P4). Degrade at zero balance applied NOTHING — not
// the recording stop, not the drain, both of which degrade exists to allow — and the
// previous version of this test asserted that as correct.
pg_test!(degrade_mode_at_zero_applies_everything_short_of_ending, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(
        *actuator.calls.lock().unwrap(),
        vec!["stop_recording:b1".to_string(), "drain_to_origin:b1".to_string()],
        "everything up to draining; not the ending"
    );
    assert_eq!(
        r.withheld,
        vec![("b1".to_string(), DemotionStep::EndWithSlate)],
        "and the ending that was decided and not applied is visible, not silent"
    );
    let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!((target.as_str(), applied.as_str()), ("end_with_slate", "drain_to_origin"));
});

pg_test!(full_mode_ends_the_broadcast_with_a_slate, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(
        *actuator.calls.lock().unwrap(),
        vec![
            "stop_recording:b1".to_string(),
            "drain_to_origin:b1".to_string(),
            "end_with_slate:b1".to_string()
        ],
        "ending is cumulative: the rungs it skipped past still apply"
    );
    let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!((target.as_str(), applied.as_str()), ("end_with_slate", "end_with_slate"));
});

// ── Reconciliation: the gap between decided and done keeps being worked ──────

// REGRESSION (P2). The rollout the docs describe — observe first, then enable — is
// exactly the path that used to fail: a rung decided in observe mode was never applied
// after the switch, because the loop only acted when the rung CHANGED.
pg_test!(switching_observe_to_degrade_applies_rungs_already_decided, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 400, 1_000);
    let actuator = RecordingActuator::default();
    let t0 = Utc::now();

    ladder_tick(&db, &billing, &actuator, LadderMode::Observe, &instant(), 100, t0)
        .await
        .expect("observe");
    assert!(actuator.calls.lock().unwrap().is_empty());

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100,
                t0 + Duration::seconds(60))
        .await
        .expect("degrade");
    assert_eq!(
        *actuator.calls.lock().unwrap(),
        vec!["stop_recording:b1".to_string()],
        "the operator switched to degrade; the rung already decided must now be applied"
    );
    let (_, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(applied, "reduce_quality");
});

// Raising degrade -> full must apply the ending the degrade mode withheld.
pg_test!(switching_degrade_to_full_applies_the_withheld_ending, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("degrade");
    ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("full");

    let calls = actuator.calls.lock().unwrap().clone();
    assert_eq!(calls.last().map(String::as_str), Some("end_with_slate:b1"), "{calls:?}");
    let (_, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(applied, "end_with_slate");
});

// REGRESSION (P3). A failed actuation must be retried. The old code's comment said it
// was; the rung had not changed, so the loop never looked at it again.
pg_test!(a_failed_actuation_is_retried_on_the_next_tick, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 400, 1_000);

    let r = ladder_tick(&db, &billing, &BrokenActuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert_eq!(r.actuation_failures.len(), 1);
    let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(target, "reduce_quality", "the decision is recorded");
    // Actuation is progressive: StopProvisioning has no side effect of its own (the
    // planner's balance gate enforces it from the same numbers), so it IS in force;
    // ReduceQuality's recording stop failed, so it is not.
    assert_eq!(applied, "stop_provisioning", "exactly what is in force, and no more");
    let st = statements(&pool, "b1").await;
    assert_eq!(
        st.iter().map(|s| s.2.as_str()).collect::<Vec<_>>(),
        vec!["stop_provisioning"],
        "a statement for the restriction that did take effect, none for the one that did not"
    );

    // The actuator recovers.
    let actuator = RecordingActuator::default();
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert_eq!(*actuator.calls.lock().unwrap(), vec!["stop_recording:b1".to_string()]);
    let (_, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(applied, "reduce_quality");
});

// REGRESSION (P5). An ending that was only DECIDED is not final. The broadcast is
// still on air; after a top-up it must recover, not stay recorded as ended forever —
// which also corrupted the forecast observe mode exists to produce.
pg_test!(a_withheld_ending_recovers_after_a_top_up, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();

    for mode in [LadderMode::Observe, LadderMode::Degrade] {
        wipe(&pool).await;
        live_broadcast(&pool, "b1", "@host:hs").await;
        billing.set("b1", 0, 1_000);
        ladder_tick(&db, &billing, &actuator, mode, &instant(), 100, Utc::now())
            .await
            .expect("zero");
        billing.set("b1", 100_000, 1_000);
        ladder_tick(&db, &billing, &actuator, mode, &instant(), 100, Utc::now())
            .await
            .expect("top-up");
        let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
        assert_eq!(
            (target.as_str(), applied.as_str()),
            ("healthy", "healthy"),
            "{mode}: never ended, topped up, and must be healthy again"
        );
    }
});

// ── CR-604 ───────────────────────────────────────────────────────────────────

// Every applied change has a statement, written in the same transaction as the state.
pg_test!(every_applied_restriction_has_its_statement_of_reasons, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();
    let t0 = Utc::now();

    for (i, (balance, want)) in [
        (400i64, "reduce_quality"),
        (100, "drain_to_origin"),
        (0, "end_with_slate"),
    ]
    .into_iter()
    .enumerate()
    {
        billing.set("b1", balance, 1_000);
        ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100,
                    t0 + Duration::seconds(60 * i as i64))
            .await
            .expect("tick");
        let (_, applied, _) = rung(&pool, "b1").await.expect("rung");
        assert_eq!(applied, want);
    }

    let st = statements(&pool, "b1").await;
    assert_eq!(
        st.iter().map(|s| s.2.as_str()).collect::<Vec<_>>(),
        vec!["reduce_quality", "drain_to_origin", "end_with_slate"],
        "one statement per applied change, none skipped"
    );
    for s in &st {
        assert!(s.3.contains("balance"), "a statement must name what it turns on");
    }
    assert_eq!(
        db.undelivered_statements().await.expect("count"),
        3,
        "an undelivered statement of reasons is a debt; decisions are not counted"
    );
});

// A top-up lifts the restriction, and the broadcaster is told — including that a
// stopped recording does not restart by itself.
pg_test!(recovery_is_an_applied_change_with_its_own_statement, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();

    billing.set("b1", 400, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("demote");
    billing.set("b1", 5_000, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("recover");

    let st = statements(&pool, "b1").await;
    assert_eq!(st.len(), 2);
    assert_eq!(st[1].2, "healthy");
    assert!(st[1].3.contains("does not restart"), "{}", st[1].3);
    assert_eq!(
        actuator.calls.lock().unwrap().len(),
        1,
        "recovery calls no actuator: nothing is physically undone"
    );
});

// ── Not acting ───────────────────────────────────────────────────────────────

// 🔴 The asymmetry with the planner: a quote the ladder cannot get must not degrade.
pg_test!(a_broadcast_that_cannot_be_priced_is_skipped_not_demoted, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.fail("b1", "no rate card for currency eur");
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(r.moved.len(), 0);
    assert_eq!(r.unpriceable.len(), 1);
    assert!(actuator.calls.lock().unwrap().is_empty());
    assert!(rung(&pool, "b1").await.is_none(), "no rung for a broadcast nobody could price");
});

pg_test!(one_unpriceable_broadcast_does_not_stop_the_others, pool, {
    live_broadcast(&pool, "b-bad", "@a:hs").await;
    live_broadcast(&pool, "b-good", "@b:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.fail("b-bad", "no rate card");
    billing.set("b-good", 400, 1_000);
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(r.evaluated, 2);
    assert_eq!(r.unpriceable.len(), 1);
    assert_eq!(*actuator.calls.lock().unwrap(), vec!["stop_recording:b-good".to_string()]);
});

// REGRESSION (review item 8). `ORDER BY id LIMIT n` evaluated the same first n
// broadcasts every tick and never reached the rest.
pg_test!(every_live_broadcast_is_evaluated_however_small_the_page, pool, {
    for b in ["b1", "b2", "b3", "b4", "b5"] {
        live_broadcast(&pool, b, "@host:hs").await;
    }
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    for b in ["b1", "b2", "b3", "b4", "b5"] {
        billing.set(b, 400, 1_000);
    }
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 2, Utc::now())
        .await
        .expect("tick");

    assert_eq!(r.evaluated, 5, "a page size of 2 must still reach all five");
    assert_eq!(actuator.calls.lock().unwrap().len(), 5);
});

// ── Hysteresis, across real ticks ────────────────────────────────────────────

pg_test!(recovery_waits_for_the_dwell_across_ticks, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();
    let policy = LadderPolicy::placeholder(); // dwell 3
    let t0 = Utc::now();

    billing.set("b1", 400, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100, t0)
        .await
        .expect("demote");
    assert_eq!(rung(&pool, "b1").await.unwrap().0, "reduce_quality");

    billing.set("b1", 5_000, 1_000);
    for i in 1..=3 {
        ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100,
                    t0 + Duration::seconds(60 * i))
            .await
            .expect("tick");
        let (target, applied, streak) = rung(&pool, "b1").await.unwrap();
        assert_eq!(target, "reduce_quality", "recovered early on evaluation {i}");
        assert_eq!(applied, "reduce_quality");
        assert_eq!(streak, i as i32, "the streak must survive in the database");
    }

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100,
                t0 + Duration::seconds(240))
        .await
        .expect("recover");
    let (target, applied, streak) = rung(&pool, "b1").await.unwrap();
    assert_eq!((target.as_str(), applied.as_str()), ("healthy", "healthy"));
    assert_eq!(streak, 0);
});

pg_test!(demotion_does_not_wait_for_any_dwell, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();
    let policy = LadderPolicy::placeholder();

    billing.set("b1", 5_000, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100, Utc::now())
        .await
        .expect("healthy");
    billing.set("b1", 100, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100, Utc::now())
        .await
        .expect("demote");
    let (target, applied, _) = rung(&pool, "b1").await.unwrap();
    assert_eq!((target.as_str(), applied.as_str()), ("drain_to_origin", "drain_to_origin"),
        "one tick, straight down");
});

// An unchanged rung writes nothing more and applies nothing more. A broadcaster
// receiving the same notice every minute is being spammed, not informed.
pg_test!(an_unchanged_rung_issues_no_further_statement, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();
    billing.set("b1", 400, 1_000);

    for _ in 0..4 {
        ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
            .await
            .expect("tick");
    }

    assert_eq!(statements(&pool, "b1").await.len(), 1, "one applied change, one statement");
    assert_eq!(events(&pool, "b1").await.len(), 2, "plus its one decision");
    assert_eq!(actuator.calls.lock().unwrap().len(), 1, "applied once, not every tick");
});

pg_test!(a_rungs_entered_at_survives_later_evaluations, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator = RecordingActuator::default();
    billing.set("b1", 400, 1_000);
    let t0 = Utc::now();

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, t0)
        .await
        .expect("tick");
    let entered: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT entered_at FROM mm_broadcast_demotion WHERE broadcast_id = 'b1'")
            .fetch_one(&pool)
            .await
            .expect("entered_at");

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100,
                t0 + Duration::seconds(600))
        .await
        .expect("tick");
    let (still, evaluated): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT entered_at, evaluated_at FROM mm_broadcast_demotion WHERE broadcast_id = 'b1'",
    )
    .fetch_one(&pool)
    .await
    .expect("times");
    assert_eq!(entered, still, "entered_at is when the rung began, not when it was last seen");
    assert!(evaluated > entered, "but evaluated_at does move");
});

pg_test!(an_ended_broadcast_is_not_evaluated, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    sqlx::query("UPDATE mm_streams SET status = 'ended' WHERE id = 'b1'")
        .execute(&pool)
        .await
        .expect("end it");
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert_eq!(r.evaluated, 0);
    assert!(actuator.calls.lock().unwrap().is_empty());
});

// Once an ending is APPLIED, a top-up must not resurrect it.
pg_test!(a_top_up_after_an_applied_slate_does_not_bring_the_broadcast_back, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator: Arc<RecordingActuator> = Arc::new(RecordingActuator::default());
    billing.set("b1", 0, 1_000);
    ladder_tick(&db, &billing, actuator.as_ref(), LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("end");

    billing.set("b1", 100_000, 1_000);
    ladder_tick(&db, &billing, actuator.as_ref(), LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    let (target, applied, _) = rung(&pool, "b1").await.unwrap();
    assert_eq!(
        (target.as_str(), applied.as_str()),
        ("end_with_slate", "end_with_slate"),
        "money arriving after the slate is a matter for the ledger, not a restart"
    );
    assert_eq!(statements(&pool, "b1").await.len(), 1, "and no second statement");
});

// ── The gauge ────────────────────────────────────────────────────────────────

// REGRESSION (P6). The gauge is "live broadcasts by rung". Every evaluated broadcast
// gets a row and rows are never deleted, so counting the table counted every
// broadcast ever live.
pg_test!(the_rung_gauge_counts_only_live_broadcasts, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    live_broadcast(&pool, "b2", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 5_000, 1_000);
    billing.set("b2", 400, 1_000);
    ladder_tick(&db, &billing, &RecordingActuator::default(), LadderMode::Observe, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    sqlx::query("UPDATE mm_streams SET status = 'ended' WHERE id = 'b1'")
        .execute(&pool)
        .await
        .expect("end b1");

    let mut counts = db.live_step_counts().await.expect("counts");
    counts.sort();
    assert_eq!(
        counts,
        vec![
            ("applied".to_string(), "healthy".to_string(), 1),
            ("target".to_string(), "reduce_quality".to_string(), 1),
        ],
        "only b2 is live: observe mode leaves it applied-healthy with a reduce_quality forecast"
    );
});

// ── Partial application ──────────────────────────────────────────────────────

/// The real actuator whenever fan-out nodes exist: the drain is not implemented, so
/// it fails honestly.
#[derive(Default)]
struct DrainNotImplemented(Mutex<Vec<String>>);

#[async_trait]
impl LadderActuator for DrainNotImplemented {
    async fn stop_recording(&self, b: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("stop_recording:{b}"));
        Ok(())
    }
    async fn drain_to_origin(&self, b: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("drain_to_origin:{b}"));
        Err("draining live viewers is not implemented".into())
    }
    async fn end_with_slate(&self, b: &str, _: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("end_with_slate:{b}"));
        Ok(())
    }
}

// The recording DID stop. The ladder must remember that, report the drain as failed,
// and keep retrying only the part that has not happened.
pg_test!(a_failed_drain_leaves_what_did_happen_applied_and_retries_the_rest, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 100, 1_000); // DrainToOrigin
    let actuator = DrainNotImplemented::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert_eq!(r.actuation_failures.len(), 1);
    let (target, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!((target.as_str(), applied.as_str()), ("drain_to_origin", "reduce_quality"));
    let st = statements(&pool, "b1").await;
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].2, "reduce_quality", "the statement is for what was done, not what was wanted");

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("retry");
    assert_eq!(
        *actuator.0.lock().unwrap(),
        vec![
            "stop_recording:b1".to_string(),
            "drain_to_origin:b1".to_string(),
            "drain_to_origin:b1".to_string()
        ],
        "the retry attempts only the drain; the recording stop is not repeated"
    );
});

// Full mode must still be able to end a broadcast whose drain cannot succeed.
pg_test!(full_mode_ends_a_broadcast_even_when_the_drain_cannot_succeed, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = DrainNotImplemented::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert!(r.actuation_failures.is_empty(), "the ending supersedes the failed drain");
    let (_, applied, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(applied, "end_with_slate");
});
