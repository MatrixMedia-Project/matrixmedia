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
#[derive(Default)]
struct FakeBilling {
    quotes: Mutex<std::collections::HashMap<String, Result<(i64, i64), String>>>,
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

async fn rung(pool: &PgPool, broadcast: &str) -> Option<(String, bool, i32)> {
    sqlx::query_as("SELECT step, actuated, milder_streak FROM mm_broadcast_demotion WHERE broadcast_id = $1")
        .bind(broadcast)
        .fetch_optional(pool)
        .await
        .expect("rung")
}

async fn statements(pool: &PgPool, broadcast: &str) -> Vec<(String, String, String, bool)> {
    sqlx::query_as(
        "SELECT from_step, to_step, statement, actuated FROM mm_demotion_events
          WHERE broadcast_id = $1 ORDER BY id",
    )
    .bind(broadcast)
    .fetch_all(pool)
    .await
    .expect("statements")
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
// record a forecast and end nothing.
pg_test!(observe_mode_records_the_rung_and_ends_nothing, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000); // Broke: the ladder wants to end it.
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Observe, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(r.evaluated, 1);
    assert_eq!(r.moved.len(), 1);
    assert!(
        actuator.calls.lock().unwrap().is_empty(),
        "the DEFAULT mode called {:?} — on a platform with no rate card that is every \
         live broadcast being terminated",
        actuator.calls.lock().unwrap()
    );

    let (step, actuated, _) = rung(&pool, "b1").await.expect("a rung was recorded");
    assert_eq!(step, "end_with_slate", "the forecast is still recorded");
    assert!(!actuated, "and it is recorded as NOT applied");

    let st = statements(&pool, "b1").await;
    assert_eq!(st.len(), 1);
    assert!(!st[0].3, "a statement about something that did not happen must say so");
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

    assert_eq!(
        *actuator.calls.lock().unwrap(),
        vec!["stop_recording:b1".to_string()],
        "recording is the one charge that keeps accruing after the broadcast ends"
    );
    let (step, actuated, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(step, "reduce_quality");
    assert!(actuated);
});

// The distinction the third mode exists for: degrade will take a broadcast all the
// way to draining viewers, and still not end it.
pg_test!(degrade_mode_withholds_the_ending_and_says_so, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 0, 1_000);
    let actuator = RecordingActuator::default();

    let r = ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert!(actuator.calls.lock().unwrap().is_empty(), "nothing at all was applied");
    assert_eq!(
        r.withheld,
        vec![("b1".to_string(), DemotionStep::EndWithSlate)],
        "a decision taken and not applied must be visible, not silent"
    );
    let (_, actuated, _) = rung(&pool, "b1").await.expect("rung");
    assert!(!actuated);
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
    let (step, actuated, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(step, "end_with_slate");
    assert!(actuated);
});

// ── CR-604 ───────────────────────────────────────────────────────────────────

// A restriction applied with no record of why is the exact failure CR-604 exists to
// prevent, and it would be invisible — so the two are one transaction.
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
        ladder_tick(
            &db,
            &billing,
            &actuator,
            LadderMode::Full,
            &instant(),
            100,
            t0 + Duration::seconds(60 * i as i64),
        )
        .await
        .expect("tick");
        let (step, _, _) = rung(&pool, "b1").await.expect("rung");
        assert_eq!(step, want);
    }

    let st = statements(&pool, "b1").await;
    assert_eq!(st.len(), 3, "one per transition, none skipped");
    assert_eq!(
        st.iter().map(|s| s.1.as_str()).collect::<Vec<_>>(),
        vec!["reduce_quality", "drain_to_origin", "end_with_slate"]
    );
    for s in &st {
        assert!(s.2.contains("balance"), "a statement must name what it turns on");
        assert!(s.3, "and these were all applied");
    }

    // It must also be countable as a compliance debt while nothing delivers it.
    assert_eq!(
        db.undelivered_statements().await.expect("count"),
        3,
        "an undelivered statement of reasons is a debt, and one that cannot be \
         counted is one nobody will pay"
    );
});

// ── Not acting ───────────────────────────────────────────────────────────────

// 🔴 The asymmetry with the planner. A quote the planner cannot get BLOCKS
// provisioning (FR-308b); a quote the ladder cannot get must not degrade anyone. Both
// default to not acting — "not acting" just means opposite things in the two places.
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
    assert!(
        rung(&pool, "b1").await.is_none(),
        "no rung may be recorded for a broadcast nobody could price"
    );
});

// One broadcast's problem must not stop the others. On the way down that matters
// more than usual: a stuck ladder keeps spending.
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
    assert_eq!(r.moved.len(), 1);
    assert_eq!(*actuator.calls.lock().unwrap(), vec!["stop_recording:b-good".to_string()]);
});

// A failed actuation must be recorded as NOT applied, or the next pass sees no change
// and never retries — leaving a restriction that was decided, never applied, and
// believed to be in force.
pg_test!(a_failed_actuation_is_recorded_as_not_applied, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    billing.set("b1", 400, 1_000);

    let r = ladder_tick(&db, &billing, &BrokenActuator, LadderMode::Degrade, &instant(), 100, Utc::now())
        .await
        .expect("tick");

    assert_eq!(r.actuation_failures.len(), 1);
    let (step, actuated, _) = rung(&pool, "b1").await.expect("rung");
    assert_eq!(step, "reduce_quality", "the rung is still recorded");
    assert!(!actuated, "but honestly, as not applied");
});

// ── Hysteresis, across real ticks ────────────────────────────────────────────

// The flapping guard, end to end: the streak has to survive in the database between
// ticks or the dwell resets every time and recovery is instant.
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

    // They top up. The rung must hold while the dwell builds.
    billing.set("b1", 5_000, 1_000);
    for i in 1..=3 {
        ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100,
                    t0 + Duration::seconds(60 * i))
            .await
            .expect("tick");
        let (step, _, streak) = rung(&pool, "b1").await.unwrap();
        assert_eq!(step, "reduce_quality", "recovered early on evaluation {i}");
        assert_eq!(streak, i as i32, "the streak must survive in the database");
    }

    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100,
                t0 + Duration::seconds(240))
        .await
        .expect("recover");
    let (step, _, streak) = rung(&pool, "b1").await.unwrap();
    assert_eq!(step, "healthy", "and then it does recover");
    assert_eq!(streak, 0);
});

// Demotion, by contrast, is immediate — the money is being spent now.
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
    assert_eq!(rung(&pool, "b1").await.unwrap().0, "healthy");

    billing.set("b1", 100, 1_000);
    ladder_tick(&db, &billing, &actuator, LadderMode::Degrade, &policy, 100, Utc::now())
        .await
        .expect("demote");
    assert_eq!(
        rung(&pool, "b1").await.unwrap().0,
        "drain_to_origin",
        "one tick, straight down"
    );
});

// An unchanged rung writes no second statement. A broadcaster receiving the same
// notice every minute is being spammed, not informed.
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

    assert_eq!(statements(&pool, "b1").await.len(), 1, "one transition, one statement");
    assert_eq!(
        actuator.calls.lock().unwrap().len(),
        1,
        "and the effect is applied once, not re-applied every tick"
    );
});

// `entered_at` must not move on a tick that changed nothing, or "degraded for forty
// minutes" becomes "degraded for one minute" forever.
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
    let still: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT entered_at FROM mm_broadcast_demotion WHERE broadcast_id = 'b1'")
            .fetch_one(&pool)
            .await
            .expect("entered_at");
    assert_eq!(entered, still, "entered_at is when the rung began, not when it was last seen");

    let evaluated: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT evaluated_at FROM mm_broadcast_demotion WHERE broadcast_id = 'b1'",
    )
    .fetch_one(&pool)
    .await
    .expect("evaluated_at");
    assert!(evaluated > entered, "but evaluated_at does move");
});

// Only live broadcasts. Demoting an ended one would write restrictions against a
// stream nobody is watching, and issue statements about it.
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

// Once ended, a top-up must not resurrect it. The ladder would otherwise report a
// stream as healthy that nothing had restarted.
pg_test!(a_top_up_after_the_slate_does_not_bring_the_broadcast_back, pool, {
    live_broadcast(&pool, "b1", "@host:hs").await;
    let db = PgLadderDb::new(pool.clone());
    let billing = FakeBilling::default();
    let actuator: Arc<RecordingActuator> = Arc::new(RecordingActuator::default());
    billing.set("b1", 0, 1_000);
    ladder_tick(&db, &billing, actuator.as_ref(), LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("end");
    assert_eq!(rung(&pool, "b1").await.unwrap().0, "end_with_slate");

    billing.set("b1", 100_000, 1_000);
    ladder_tick(&db, &billing, actuator.as_ref(), LadderMode::Full, &instant(), 100, Utc::now())
        .await
        .expect("tick");
    assert_eq!(
        rung(&pool, "b1").await.unwrap().0,
        "end_with_slate",
        "money arriving after the slate is a matter for the ledger, not a restart"
    );
    assert_eq!(statements(&pool, "b1").await.len(), 1, "and no second statement");
});
