//! PG-gated coverage for the reconcile loop (WS-B Task B5).
//!
//! The behaviour worth testing is what each `FleetMode` does and, more
//! importantly, what it does NOT do: `frozen` must not provision, `off` must tear
//! down through the normal path so the deadline counters stay at zero, and a
//! census failure must not be mistaken for "nothing is live".

use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use mm_core::config::FleetMode;
use mm_core::fleet::planner::FleetPolicy;
use mm_core::fleet::{NodeState, Ownership};
use mm_core::metrics_global::FLEET_REAPER_DEADLINE_KILLS;
use mm_fleet::desired::DesiredStore;
use mm_fleet::provider::{DryRunProvider, Intent};
use mm_fleet::runner::{
    provision_seconds, BillingSource, BroadcastBilling, BroadcastCensus, FleetRunner, LiveBroadcast,
    NoBillingYet,
};
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

fn runner_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}

async fn wipe(pool: &PgPool) {
    for table in ["mm_fleet_desired", "mm_fleet_nodes"] {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("wipe {table}: {e}"));
    }
}

async fn insert_node(pool: &PgPool, id: &str, ownership: Ownership, state: NodeState) {
    let deadline = ownership
        .requires_destroy_deadline()
        .then(|| Utc::now() + Duration::hours(3));
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, provider_id, state, destroy_deadline, renewal_due_at)
         VALUES ($1, 'fanout', $2, 'dry-run', $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(ownership.as_str())
    .bind(format!("prov-{id}"))
    .bind(state.as_str())
    .bind(deadline)
    .bind((ownership == Ownership::Leased).then(|| Utc::now() + Duration::days(30)))
    .execute(pool)
    .await
    .expect("insert node");
}

async fn insert_desired(pool: &PgPool, id: &str, broadcast: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired
             (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline)
         VALUES ($1, 'fanout', 'rented', 'eu-ams', 'small', $2, now() + interval '3 hours')",
    )
    .bind(id)
    .bind(broadcast)
    .execute(pool)
    .await
    .expect("insert desired");
}

// ── fakes ───────────────────────────────────────────────────────────────────

struct FakeCensus {
    live: Mutex<Vec<LiveBroadcast>>,
    programme_live: bool,
    fail: bool,
}

impl FakeCensus {
    fn with(live: &[(&str, u32)]) -> Self {
        Self {
            live: Mutex::new(
                live.iter()
                    .map(|(id, v)| LiveBroadcast {
                        broadcast_id: (*id).into(),
                        viewers: *v,
                    })
                    .collect(),
            ),
            programme_live: true,
            fail: false,
        }
    }
    fn failing() -> Self {
        Self {
            live: Mutex::new(vec![]),
            programme_live: true,
            fail: true,
        }
    }
}

#[async_trait]
impl BroadcastCensus for FakeCensus {
    async fn live_broadcasts(&self) -> Result<Vec<LiveBroadcast>, String> {
        if self.fail {
            return Err("switch unreachable".into());
        }
        Ok(self.live.lock().unwrap().clone())
    }
    async fn programme_is_live(&self, _broadcast_id: &str) -> Result<bool, String> {
        Ok(self.programme_live)
    }
}

struct RichWallet;

#[async_trait]
impl BillingSource for RichWallet {
    async fn quote(&self, _broadcast_id: &str) -> Result<BroadcastBilling, String> {
        Ok(BroadcastBilling {
            available_balance_minor: 10_000_000,
            projected_cost_minor: 1_000,
            transcode_enabled: false,
        })
    }
}

fn policy() -> FleetPolicy {
    FleetPolicy::conservative("eu-ams", "small")
}

fn deadline_kills() -> u64 {
    FLEET_REAPER_DEADLINE_KILLS
        .with_label_values(&["fanout"])
        .get()
}

// ── frozen: the default, and the one every existing install will run ─────────

#[tokio::test]
async fn frozen_observes_and_publishes_but_provisions_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping frozen_observes_and_publishes_but_provisions_nothing");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 1_500)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::Frozen, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.mode, "frozen");
    assert_eq!(report.nodes_observed, 1, "frozen must still OBSERVE — a subsystem \
        you cannot see is one you cannot decide to unfreeze");
    assert!(report.planned.is_empty(), "frozen must plan nothing: {:?}", report.planned);
    assert!(
        provider.intents().is_empty(),
        "frozen must not touch the provider: {:?}",
        provider.intents()
    );

    // 1,500 viewers over 250-per-node would be six nodes on `on`.
    let store = DesiredStore::new(pool.clone());
    let ids: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["bc-b1-fanout-0"],
        "frozen must not grow the desired set"
    );
}

/// `frozen` stops provisioning. It does not mean "keep paying for broadcasts that
/// have finished".
#[tokio::test]
async fn frozen_still_releases_a_finished_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping frozen_still_releases_a_finished_broadcast");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-gone-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-gone-fanout-0", "gone").await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[])), // nothing is live
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::Frozen, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.torn_down, vec!["bc-gone-fanout-0"]);
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-bc-gone-fanout-0".into())]
    );
}

// ── off: the hard stop ───────────────────────────────────────────────────────

/// E-16 / test-plan A2.4: `off` drains through the NORMAL path, so the deadline
/// counters stay at zero. That is what distinguishes "an operator stopped the
/// fleet" from "the backstop caught a leak" — and the latter is alerted on.
#[tokio::test]
async fn off_tears_down_rented_nodes_and_the_deadline_counter_stays_zero() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping off_tears_down_rented_nodes_and_the_deadline_counter_stays_zero");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_node(&pool, "bc-b1-fanout-1", Ownership::Rented, NodeState::Booting).await;
    insert_node(&pool, "origin-owned", Ownership::Owned, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let before = deadline_kills();

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 1_500)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::Off, Utc::now())
        .await
        .expect("tick");

    let mut torn = report.torn_down.clone();
    torn.sort();
    assert_eq!(torn, vec!["bc-b1-fanout-0", "bc-b1-fanout-1"]);
    assert!(
        !report.torn_down.iter().any(|n| n == "origin-owned"),
        "off must not destroy owned hardware"
    );
    assert_eq!(
        deadline_kills(),
        before,
        "an operator stopping the fleet must NOT increment the deadline counter — \
         that counter is alerted on and means the backstop caught a leak"
    );
    assert!(report.planned.is_empty(), "off must plan nothing");
}

// ── on: the only mode that spends ────────────────────────────────────────────

#[tokio::test]
async fn on_writes_a_desired_set_for_a_live_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping on_writes_a_desired_set_for_a_live_broadcast");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 600)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");
    assert_eq!(report.planned, vec!["b1"]);

    let store = DesiredStore::new(pool.clone());
    let rows = store.load_all().await.expect("load");
    assert_eq!(rows.len(), 3, "600 viewers over 250-per-node is 3 nodes: {rows:?}");
    for r in &rows {
        assert_eq!(r.ownership, Ownership::Rented);
        assert!(
            r.destroy_deadline.is_some(),
            "every rented row must carry a deadline written BEFORE any provider call"
        );
    }
}

/// THE SAFETY PROPERTY OF THIS WHOLE TASK.
///
/// The default billing source refuses to quote, so the runner cannot provision
/// before the wallet that authorises spending exists. Not a convention — the code
/// path.
#[tokio::test]
async fn the_default_billing_source_makes_provisioning_impossible() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_default_billing_source_makes_provisioning_impossible");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 100_000)])),
        Box::new(NoBillingYet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert!(report.planned.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(
        report.skipped[0].1.contains("WS-D"),
        "the skip reason must name what is missing: {:?}",
        report.skipped
    );
    assert!(
        DesiredStore::new(pool.clone())
            .load_all()
            .await
            .expect("load")
            .is_empty(),
        "100,000 viewers and a rich-looking mode must still provision nothing \
         without a wallet"
    );
}

/// A census failure must not read as "no broadcasts are live" — that would tear
/// the entire fleet down on a transient switch error.
#[tokio::test]
async fn a_census_failure_tears_nothing_down() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_census_failure_tears_nothing_down");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::failing()),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("a census failure is reported in the tick, not returned as Err");

    assert!(
        report.torn_down.is_empty(),
        "a transient census failure destroyed nodes: {:?}",
        report.torn_down
    );
    assert!(provider.intents().is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].1.contains("census unavailable"));
}

/// FR-341: the mode is an argument, so flipping it takes effect on the next tick
/// with no restart. One runner, three modes.
#[tokio::test]
async fn a_mode_change_takes_effect_on_the_next_tick() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_mode_change_takes_effect_on_the_next_tick");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 600)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    runner
        .tick(&provider, FleetMode::Frozen, Utc::now())
        .await
        .expect("frozen tick");
    assert!(store.load_all().await.expect("load").is_empty(), "frozen: nothing");

    runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("on tick");
    assert_eq!(store.load_all().await.expect("load").len(), 3, "on: three nodes");

    runner
        .tick(&provider, FleetMode::Frozen, Utc::now())
        .await
        .expect("frozen again");
    assert_eq!(
        store.load_all().await.expect("load").len(),
        3,
        "re-freezing must not destroy what `on` provisioned — freezing is not a \
         teardown"
    );
}

/// Ticking twice on an unchanged observation must not grow the fleet — the
/// planner is idempotent and the runner must not undo that.
#[tokio::test]
async fn two_identical_ticks_provision_the_same_set() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping two_identical_ticks_provision_the_same_set");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 600)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("tick 1");
    let first: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();

    runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("tick 2");
    let second: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();

    assert_eq!(first, second, "a repeated tick ordered more capacity");
    assert_eq!(first.len(), 3);
}

// ── the provision-time helper (pure) ─────────────────────────────────────────

#[test]
fn provision_seconds_measures_the_interval() {
    let now = Utc::now();
    let got = provision_seconds(Some(now - Duration::seconds(42)), now).expect("some");
    assert!((got - 42.0).abs() < 0.01, "got {got}");
}

#[test]
fn provision_seconds_is_none_without_a_request_time() {
    assert!(provision_seconds(None, Utc::now()).is_none());
}

/// A negative interval means the clocks disagree. Discard it rather than clamping
/// to zero: a pile of zeroes reads as "provisioning is instant", which is a worse
/// lie than a missing sample — and this histogram is the only measurement of
/// provision-to-ready we will have.
#[test]
fn a_negative_interval_is_discarded_not_clamped() {
    let now = Utc::now();
    assert!(provision_seconds(Some(now + Duration::seconds(5)), now).is_none());
}

/// The capacity a healthy node is already providing must reach the planner, or the
/// runner orders a replacement for a machine that is serving fine.
#[tokio::test]
async fn a_healthy_nodes_capacity_is_counted_and_not_re_ordered() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_healthy_nodes_capacity_is_counted_and_not_re_ordered");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    // One healthy node with room for 250 and 100 on it: 150 spare.
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, provider_id, state,
              destroy_deadline, viewer_capacity, viewers_current)
         VALUES ('bc-b1-fanout-0', 'fanout', 'rented', 'dry-run', 'prov-0', 'healthy',
                 now() + interval '3 hours', 250, 100)",
    )
    .execute(&pool)
    .await
    .expect("insert node");
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let store = DesiredStore::new(pool.clone());
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        // 150 viewers fit in the spare capacity exactly.
        Box::new(FakeCensus::with(&[("b1", 150)])),
        Box::new(RichWallet),
        policy(),
    );
    let provider = DryRunProvider::default();

    runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("tick");

    let ids: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["bc-b1-fanout-0"],
        "the existing node's 150 spare slots cover the demand; ordering more pays \
         twice for capacity we already have"
    );
}
