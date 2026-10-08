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
use mm_core::fleet::billing::BillingIncrement;
use mm_core::fleet::planner::FleetPolicy;
use mm_core::fleet::{NodeState, Ownership};
use mm_core::metrics_global::FLEET_REAPER_DEADLINE_KILLS;
use mm_fleet::desired::DesiredStore;
use mm_fleet::provider::{
    DryRunProvider, InstanceHandle, InstanceSpec, Intent, Provider, ProviderError,
};
use mm_core::fleet::transcode::{TranscodeOptIn, TranscodeOverride};
use mm_fleet::runner::{
    provision_seconds, BillingSource, BroadcastBilling, BroadcastCensus, FleetRunner, LiveBroadcast,
    NoBillingYet, PgTranscodeOptIns, TranscodeOptIns,
};
use mm_fleet::sweeper::sweep_deadlines;
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
            broadcaster_is_paying: true,
            transcoder_cost_minor: 240,
        })
    }
}

/// The state of every broadcast before a broadcaster chooses anything.
struct NobodyOptedIn;

#[async_trait]
impl TranscodeOptIns for NobodyOptedIn {
    async fn opt_in(&self, _broadcast_id: &str) -> Result<TranscodeOptIn, String> {
        Ok(TranscodeOptIn::default())
    }
}

/// A provider that can run transcode software is available in every region.
struct SupplyReady;

#[async_trait]
impl mm_fleet::runner::TranscodeSupply for SupplyReady {
    async fn ready(&self, _region: &str) -> Result<bool, String> {
        Ok(true)
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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

/// A census failure stops the tick from deciding anything — but not from telling
/// Terraform what the database already says. The render needs no census, and a
/// teardown that happened while the census was down (the deadline sweeper runs on
/// its own loop) must not sit in the file for the whole outage: until it leaves,
/// the next `terraform apply` would create the machine again.
#[tokio::test]
async fn a_census_failure_still_renders_what_was_torn_down() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_census_failure_still_renders_what_was_torn_down");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    sqlx::query(
        "UPDATE mm_fleet_nodes SET destroy_deadline = now() - interval '1 minute'
          WHERE mm_node_id = 'bc-b1-fanout-0'",
    )
    .execute(&pool)
    .await
    .expect("expire the deadline");
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let dir = tf_tmpdir("census-down");
    let provider = DryRunProvider::default();
    let healthy = runner_rendering_to(&pool, &dir, FakeCensus::with(&[("b1", 0)]), Box::new(NobodyOptedIn));
    healthy.tick(&provider, FleetMode::On, Utc::now()).await.expect("first tick");
    assert_eq!(tf_keys(&dir), vec!["bc-b1-fanout-0"]);

    // The census goes down; meanwhile the deadline sweeper tears the node down.
    let swept = sweep_deadlines(
        &DesiredStore::new(pool.clone()),
        &provider,
        BillingIncrement::PerHour,
        Utc::now(),
    )
    .await
    .expect("sweep");
    assert_eq!(swept.reaped, vec!["bc-b1-fanout-0"]);

    let blind = runner_rendering_to(&pool, &dir, FakeCensus::failing(), Box::new(NobodyOptedIn));
    let report = blind.tick(&provider, FleetMode::On, Utc::now()).await.expect("census-down tick");
    assert!(report.skipped[0].1.contains("census unavailable"), "{:?}", report.skipped);
    assert!(report.torn_down.is_empty(), "a census failure must still tear nothing down");
    assert_eq!(
        report.tfvars_nodes,
        Some(0),
        "a census failure skipped the render, so the file goes on naming a node the \
         sweeper destroyed"
    );
    assert!(tf_keys(&dir).is_empty(), "{:?}", tf_keys(&dir));
    std::fs::remove_dir_all(&dir).ok();
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
        Box::new(NobodyOptedIn),
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
        Box::new(NobodyOptedIn),
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

    // One healthy node with room for 250 and 100 on it.
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
        // The census counts the whole audience: the 100 on the node and 150 more
        // fill its 250 exactly.
        Box::new(FakeCensus::with(&[("b1", 250)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
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
        "the existing node's 250 slots cover the 250 viewers; ordering more pays \
         twice for capacity we already have"
    );
}

/// The census reports a broadcast's WHOLE audience — origin and every fan-out node
/// (`SwitchCensus::live_broadcasts`) — and the nodes report the viewers seated on
/// them. A planner that weighs the first against the nodes' spare slots counts each
/// seated viewer twice: 500 viewers filling two 250-seat nodes read as 500 more to
/// place, and two more machines are ordered for an audience that is already served.
#[tokio::test]
async fn a_seated_audience_is_not_ordered_a_second_time() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_seated_audience_is_not_ordered_a_second_time");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    for id in ["bc-b1-fanout-0", "bc-b1-fanout-1"] {
        sqlx::query(
            "INSERT INTO mm_fleet_nodes
                 (mm_node_id, flavor, ownership, provider, provider_id, state,
                  destroy_deadline, viewer_capacity, viewers_current)
             VALUES ($1, 'fanout', 'rented', 'dry-run', $2, 'healthy',
                     now() + interval '3 hours', 250, 250)",
        )
        .bind(id)
        .bind(format!("prov-{id}"))
        .execute(&pool)
        .await
        .expect("insert node");
        insert_desired(&pool, id, "b1").await;
    }

    let store = DesiredStore::new(pool.clone());
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        // The same 500 people the two nodes report as seated.
        Box::new(FakeCensus::with(&[("b1", 500)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
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
        vec!["bc-b1-fanout-0", "bc-b1-fanout-1"],
        "500 viewers already seated on 2 × 250 is a fleet that fits; anything more is \
         paying by the hour for the same audience twice"
    );
}

/// A node can be healthy before it has reported its capacity: the column is NULL
/// until it does, and the store reads NULL as 0. That 0 must not read as "this
/// machine holds nobody" — the runner would order a replacement every tick for a
/// node that is up and billing.
#[tokio::test]
async fn a_healthy_node_with_no_reported_capacity_is_not_re_ordered() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_healthy_node_with_no_reported_capacity_is_not_re_ordered");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    // `insert_node` leaves viewer_capacity NULL: healthy, capacity not yet reported.
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let store = DesiredStore::new(pool.clone());
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        // Within the policy's 250 per node.
        Box::new(FakeCensus::with(&[("b1", 200)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
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
        "an unreported capacity is assumed to be the policy's, as for a booting node; \
         a second machine here bills for an audience the first one already covers"
    );
}

// ── B6: the tick renders the desired set for Terraform ───────────────────────

fn tf_tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mm-runner-tf-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

#[tokio::test]
async fn a_tick_renders_the_desired_set_for_terraform() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_tick_renders_the_desired_set_for_terraform");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let dir = tf_tmpdir("render");
    let writer = mm_fleet::tfvars::TfvarsWriter::new(&dir);
    let path = writer.path().to_path_buf();

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 600)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    )
    .with_tfvars(writer);
    let provider = DryRunProvider::default();

    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.tfvars_nodes, Some(3));
    let text = std::fs::read_to_string(&path).expect("the file must exist after a tick");
    let parsed: mm_fleet::tfvars::Tfvars = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(parsed.len(), 3);
    for (id, node) in &parsed.desired_nodes {
        assert!(id.starts_with("bc-b1-fanout-"), "unexpected key {id}");
        assert_eq!(node.ownership, "rented");
        assert!(
            node.destroy_deadline.is_some(),
            "Terraform's own validation refuses a rented node with no deadline, so \
             rendering one would wedge every apply"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// `off` is the one caller allowed past the shrink guard: removing the whole fleet
/// is the instruction, not a symptom of a partial read.
#[tokio::test]
async fn off_renders_an_empty_set_past_the_shrink_guard() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping off_renders_an_empty_set_past_the_shrink_guard");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let dir = tf_tmpdir("off");
    let writer = mm_fleet::tfvars::TfvarsWriter::new(&dir);
    let path = writer.path().to_path_buf();

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 1_200)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    )
    .with_tfvars(writer);
    let provider = DryRunProvider::default();

    // Grow to five nodes first.
    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("on tick");
    assert_eq!(report.tfvars_nodes, Some(5), "1200 viewers / 250 = 5 nodes");

    // The nodes exist as rows so `off` has something reapable to tear down.
    for i in 0..5 {
        insert_node(
            &pool,
            &format!("bc-b1-fanout-{i}"),
            Ownership::Rented,
            NodeState::Healthy,
        )
        .await;
    }

    let report = runner
        .tick(&provider, FleetMode::Off, Utc::now())
        .await
        .expect("off tick");
    assert_eq!(report.torn_down.len(), 5);
    assert_eq!(
        report.tfvars_nodes,
        Some(0),
        "off must get past the shrink guard — a 100% removal is the instruction"
    );

    let text = std::fs::read_to_string(&path).expect("read");
    assert!(text.contains(r#""desired_nodes": {}"#), "got {text}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A tick with no Terraform directory configured must not invent one, and
/// `tfvars_nodes` must stay `None` — which is a different fact from `Some(0)`.
#[tokio::test]
async fn a_runner_without_a_terraform_directory_renders_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_runner_without_a_terraform_directory_renders_nothing");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 600)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    );
    let report = runner
        .tick(&DryRunProvider::default(), FleetMode::On, Utc::now())
        .await
        .expect("tick");
    assert_eq!(report.tfvars_nodes, None);
}

// ── B6: an explicit teardown reaches the Terraform file ──────────────────────
//
// The shrink guard judges a write against the file on disk. A teardown of 1 of 1
// rendered nodes is a 100% "shrink", so before the fix it was refused on that tick
// and every later one: the file kept naming the destroyed node, and the next
// `terraform apply` would CREATE a new paid machine under its id — which the orphan
// sweeper then destroys, and the apply after that re-creates.

fn runner_rendering_to(
    pool: &PgPool,
    dir: &std::path::Path,
    census: FakeCensus,
    transcode: Box<dyn TranscodeOptIns>,
) -> FleetRunner {
    FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(census),
        Box::new(RichWallet),
        transcode,
        policy(),
    )
    .with_tfvars(mm_fleet::tfvars::TfvarsWriter::new(dir))
}

/// What Terraform would read right now.
fn tf_keys(dir: &std::path::Path) -> Vec<String> {
    mm_fleet::tfvars::TfvarsWriter::new(dir)
        .read_current()
        .expect("read tfvars")
        .desired_nodes
        .into_keys()
        .collect()
}

#[tokio::test]
async fn a_broadcast_end_teardown_of_a_lone_node_reaches_the_tfvars_file() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_broadcast_end_teardown_of_a_lone_node_reaches_the_tfvars_file");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let dir = tf_tmpdir("lone-end");
    let provider = DryRunProvider::default();

    let on_air = runner_rendering_to(&pool, &dir, FakeCensus::with(&[("b1", 0)]), Box::new(NobodyOptedIn));
    let report = on_air.tick(&provider, FleetMode::On, Utc::now()).await.expect("on-air tick");
    assert_eq!(report.tfvars_nodes, Some(1));
    assert_eq!(tf_keys(&dir), vec!["bc-b1-fanout-0"]);

    // The broadcast ends: the runner tears its lone node down.
    let ended = runner_rendering_to(&pool, &dir, FakeCensus::with(&[]), Box::new(NobodyOptedIn));
    let report = ended.tick(&provider, FleetMode::On, Utc::now()).await.expect("end tick");
    assert_eq!(report.torn_down, vec!["bc-b1-fanout-0"]);
    assert_eq!(
        report.tfvars_nodes,
        Some(0),
        "the render was refused: a teardown the runner itself performed was judged \
         an unexplained shrink"
    );
    assert!(
        tf_keys(&dir).is_empty(),
        "the file still names the destroyed node — the next apply re-creates it: {:?}",
        tf_keys(&dir)
    );

    // Settled: nothing more to tear down, and the empty file stays empty.
    let report = ended.tick(&provider, FleetMode::On, Utc::now()).await.expect("next tick");
    assert!(report.torn_down.is_empty(), "{:?}", report.torn_down);
    assert_eq!(report.tfvars_nodes, Some(0));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_released_lone_transcoder_is_dropped_from_the_tfvars_file() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_released_lone_transcoder_is_dropped_from_the_tfvars_file");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txtf").await;
    mm_db::transcode_db::set_broadcast_override(&pool, "txtf", "@host-txtf:hs", TranscodeOverride::On)
        .await
        .expect("db")
        .expect("opt in");

    let dir = tf_tmpdir("lone-transcoder");
    let provider = DryRunProvider::default();
    let runner = runner_rendering_to(
        &pool,
        &dir,
        FakeCensus::with(&[("txtf", 0)]),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
    );
    let tick = || async { runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("tick") };

    tick().await;
    insert_transcoder(&pool, "bc-txtf-transcode-0", NodeState::Healthy).await;
    let report = tick().await;
    assert_eq!(report.tfvars_nodes, Some(1));
    assert_eq!(tf_keys(&dir), vec!["bc-txtf-transcode-0"]);

    // FR-314c release: plan_one tears the GPU down explicitly.
    sqlx::query("UPDATE mm_streams SET transcode_released = true WHERE id = 'txtf'")
        .execute(&pool)
        .await
        .expect("release");
    let report = tick().await;
    assert_eq!(report.torn_down, vec!["bc-txtf-transcode-0"]);
    assert_eq!(
        report.tfvars_nodes,
        Some(0),
        "the release's own teardown was refused by the shrink guard"
    );
    assert!(
        tf_keys(&dir).is_empty(),
        "the file still names the released GPU — the next apply re-creates it: {:?}",
        tf_keys(&dir)
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The deadline sweeper runs on its own loop, NOT inside a runner tick, so the
/// runner cannot learn about its teardown from anything it did itself. The next
/// tick must still drop the swept node — and let its replacement in: before the
/// fix the refused render kept the dead node in the file AND kept the new one out.
#[tokio::test]
async fn a_deadline_swept_lone_node_is_replaced_in_the_tfvars_file_on_the_next_tick() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_deadline_swept_lone_node_is_replaced_in_the_tfvars_file_on_the_next_tick");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    sqlx::query(
        "UPDATE mm_fleet_nodes SET destroy_deadline = now() - interval '1 minute'
          WHERE mm_node_id = 'bc-b1-fanout-0'",
    )
    .execute(&pool)
    .await
    .expect("expire the deadline");
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let dir = tf_tmpdir("deadline-swept");
    let provider = DryRunProvider::default();

    let quiet = runner_rendering_to(&pool, &dir, FakeCensus::with(&[("b1", 0)]), Box::new(NobodyOptedIn));
    quiet.tick(&provider, FleetMode::On, Utc::now()).await.expect("first tick");
    assert_eq!(tf_keys(&dir), vec!["bc-b1-fanout-0"]);

    let swept = sweep_deadlines(
        &DesiredStore::new(pool.clone()),
        &provider,
        BillingIncrement::PerHour,
        Utc::now(),
    )
    .await
    .expect("sweep");
    assert_eq!(swept.reaped, vec!["bc-b1-fanout-0"]);

    // Viewers are still there: the planner orders a replacement under a fresh id.
    let busy = runner_rendering_to(&pool, &dir, FakeCensus::with(&[("b1", 100)]), Box::new(NobodyOptedIn));
    let report = busy.tick(&provider, FleetMode::On, Utc::now()).await.expect("next tick");
    assert!(report.torn_down.is_empty(), "{:?}", report.torn_down);
    assert_eq!(
        tf_keys(&dir),
        vec!["bc-b1-fanout-1"],
        "Terraform must see the swept node gone and its replacement wanted"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A provider whose destroy never returns. Dropping the teardown future at that
/// await is, as far as the database can tell, the process dying mid-destroy.
struct HangingProvider;

#[async_trait]
impl Provider for HangingProvider {
    fn name(&self) -> &'static str {
        "hanging"
    }
    async fn create(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        unimplemented!("never creates")
    }
    async fn destroy(&self, _provider_id: &str) -> Result<(), ProviderError> {
        std::future::pending().await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
}

/// A teardown that dies inside the provider call — a killed process, a deploy
/// restart — must still leave its node unwanted. The machine may already be gone,
/// so a desired row for it is an instruction to create a new one; and with its
/// broadcast still live, the planner sees a node that looks healthy and re-states
/// exactly that row.
#[tokio::test]
async fn a_teardown_that_dies_mid_destroy_is_neither_restated_nor_left_in_the_tfvars_file() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_teardown_that_dies_mid_destroy_is_neither_restated_nor_left_in_the_tfvars_file");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let dir = tf_tmpdir("died-mid-destroy");
    let provider = DryRunProvider::default();
    let runner = runner_rendering_to(&pool, &dir, FakeCensus::with(&[("b1", 0)]), Box::new(NobodyOptedIn));
    runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("first tick");
    assert_eq!(tf_keys(&dir), vec!["bc-b1-fanout-0"]);

    // Another loop (the deadline sweeper, say) starts tearing it down and dies
    // inside the provider call.
    let store = DesiredStore::new(pool.clone());
    let node = store
        .load_nodes()
        .await
        .expect("load nodes")
        .into_iter()
        .find(|n| n.mm_node_id.as_str() == "bc-b1-fanout-0")
        .expect("the node");
    let died = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        store.teardown(&HangingProvider, &node.teardown_target()),
    )
    .await;
    assert!(died.is_err(), "the destroy must still have been in flight");

    let report = runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("next tick");
    let desired: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert!(
        desired.is_empty(),
        "the planner re-stated a node whose teardown had begun — a desired row for a \
         machine that may already be destroyed: {desired:?}"
    );
    assert_eq!(report.tfvars_nodes, Some(0));
    assert!(tf_keys(&dir).is_empty(), "{:?}", tf_keys(&dir));
    std::fs::remove_dir_all(&dir).ok();
}

/// Tears one node down from inside `quote()` — after the tick's node snapshot and
/// before its upsert — standing in for the deadline sweeper's own loop landing in
/// exactly that gap.
struct TearsDownDuringQuote {
    pool: PgPool,
    node: &'static str,
}

#[async_trait]
impl BillingSource for TearsDownDuringQuote {
    async fn quote(&self, broadcast_id: &str) -> Result<BroadcastBilling, String> {
        let store = DesiredStore::new(self.pool.clone());
        let node = store
            .load_nodes()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|n| n.mm_node_id.as_str() == self.node && n.state != NodeState::Gone);
        if let Some(node) = node {
            store
                .teardown(&DryRunProvider::default(), &node.teardown_target())
                .await
                .map_err(|e| e.to_string())?;
        }
        RichWallet.quote(broadcast_id).await
    }
}

/// The tick plans from the node snapshot it took at its start. A node torn down
/// after that snapshot still looks healthy to the plan, which re-states it — a
/// desired row for a machine that was just destroyed, which the next apply
/// creates. The desired store must refuse it.
#[tokio::test]
async fn a_node_torn_down_mid_tick_is_not_restated_from_the_stale_snapshot() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_node_torn_down_mid_tick_is_not_restated_from_the_stale_snapshot");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let dir = tf_tmpdir("stale-snapshot");
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 0)])),
        Box::new(TearsDownDuringQuote { pool: pool.clone(), node: "bc-b1-fanout-0" }),
        Box::new(NobodyOptedIn),
        policy(),
    )
    .with_tfvars(mm_fleet::tfvars::TfvarsWriter::new(&dir));
    let report = runner
        .tick(&DryRunProvider::default(), FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.planned, vec!["b1"], "{:?}", report.skipped);
    assert_eq!(node_state(&pool, "bc-b1-fanout-0").await, "gone");
    let desired: Vec<String> = DesiredStore::new(pool.clone())
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert!(
        desired.is_empty(),
        "the tick re-stated, from its stale snapshot, a node torn down mid-tick: {desired:?}"
    );
    assert_eq!(report.tfvars_nodes, Some(0));
    std::fs::remove_dir_all(&dir).ok();
}

// ── FR-314a/b/c: the transcode opt-in, end to end ────────────────────────────

/// An active broadcast row for the real opt-in lookup to read. Its id is unique to
/// the transcode tests, and it is re-created so a previous run's choice cannot leak.
async fn live_stream_row(pool: &PgPool, broadcast: &str) {
    let room = format!("!runner-{broadcast}:hs");
    sqlx::query("DELETE FROM mm_streams WHERE id = $1")
        .bind(broadcast)
        .execute(pool)
        .await
        .expect("clear stream");
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&room)
        .execute(pool)
        .await
        .expect("room");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status)
         SELECT $1, id, $2, 'active' FROM mm_rooms WHERE matrix_room_id = $3",
    )
    .bind(broadcast)
    .bind(format!("@host-{broadcast}:hs"))
    .bind(&room)
    .execute(pool)
    .await
    .expect("stream");
}

async fn insert_transcoder(pool: &PgPool, id: &str, state: NodeState) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, provider_id, state, destroy_deadline)
         VALUES ($1, 'transcode', 'rented', 'dry-run', $2, $3, now() + interval '3 hours')
         ON CONFLICT (mm_node_id) DO UPDATE SET state = EXCLUDED.state",
    )
    .bind(id)
    .bind(format!("prov-{id}"))
    .bind(state.as_str())
    .execute(pool)
    .await
    .expect("insert transcoder");
}

async fn node_state(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("node state")
}

async fn desired_transcoders(pool: &PgPool) -> Vec<String> {
    let mut ids: Vec<String> = DesiredStore::new(pool.clone())
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .filter(|r| r.flavor == mm_core::fleet::NodeFlavor::Transcode)
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

/// FR-314b: with the stored opt-in in place of the balance proxy, two equally
/// funded broadcasts differ only in their broadcaster's choice — and only the one
/// that opted in gets a GPU.
#[tokio::test]
async fn on_provisions_a_transcoder_only_where_the_broadcaster_opted_in() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping on_provisions_a_transcoder_only_where_the_broadcaster_opted_in");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txin").await;
    live_stream_row(&pool, "txout").await;
    mm_db::transcode_db::set_broadcast_override(&pool, "txin", "@host-txin:hs", TranscodeOverride::On)
        .await
        .expect("db")
        .expect("host may opt in");

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txin", 0), ("txout", 0)])),
        Box::new(RichWallet), // both funded: the proxy would have given both a GPU
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    )
    .with_transcode_supply(Box::new(SupplyReady));
    let report = runner
        .tick(&DryRunProvider::default(), FleetMode::On, Utc::now())
        .await
        .expect("tick");
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(desired_transcoders(&pool).await, vec!["bc-txin-transcode-0"]);
    assert!(
        report.not_promoted.is_empty(),
        "a broadcast that got its transcoder was promoted: {:?}",
        report.not_promoted
    );
}

/// Spec §6.4: the broadcaster opted in and can pay, but no provider has transcode software, so
/// the broadcast stays on the origin's single layer — and the tick says why.
#[tokio::test]
async fn an_opted_in_broadcast_is_reported_not_promoted_while_no_provider_has_transcode_software() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping an_opted_in_broadcast_is_reported_not_promoted_while_no_provider_has_transcode_software"
        );
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txsw").await;
    mm_db::transcode_db::set_broadcast_override(
        &pool,
        "txsw",
        "@host-txsw:hs",
        TranscodeOverride::On,
    )
    .await
    .expect("db")
    .expect("host may opt in");
    // No `.with_transcode_supply(...)`: the default supply is none.
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txsw", 0)])),
        Box::new(RichWallet),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    );
    let report = runner
        .tick(&DryRunProvider::default(), FleetMode::On, Utc::now())
        .await
        .expect("tick");
    assert!(desired_transcoders(&pool).await.is_empty());
    assert_eq!(
        report.not_promoted,
        vec![("txsw".to_string(), "transcode_software_not_configured")]
    );
}

/// A closed gate stops growth and never destroys: the transcoder this broadcast already runs
/// stays desired and running, and the broadcast is not reported as waiting for anything.
#[tokio::test]
async fn a_closed_gate_leaves_the_transcoder_that_already_runs_alone() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_closed_gate_leaves_the_transcoder_that_already_runs_alone"
        );
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txkeep").await;
    mm_db::transcode_db::set_broadcast_override(
        &pool,
        "txkeep",
        "@host-txkeep:hs",
        TranscodeOverride::On,
    )
    .await
    .expect("db")
    .expect("host may opt in");
    insert_transcoder(&pool, "bc-txkeep-transcode-0", NodeState::Healthy).await;
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txkeep", 0)])),
        Box::new(RichWallet),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    ); // the default supply is none: the gate is closed
    let provider = DryRunProvider::default();
    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert_eq!(
        desired_transcoders(&pool).await,
        vec!["bc-txkeep-transcode-0"]
    );
    assert!(report.torn_down.is_empty(), "{:?}", report.torn_down);
    assert!(provider.intents().is_empty(), "{:?}", provider.intents());
    assert_eq!(node_state(&pool, "bc-txkeep-transcode-0").await, "healthy");
    assert!(
        report.not_promoted.is_empty(),
        "it already has one: {:?}",
        report.not_promoted
    );
}

/// Answers as told and remembers which regions it was asked about.
struct ScriptedSupply {
    answer: Result<bool, String>,
    asked: std::sync::Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl mm_fleet::runner::TranscodeSupply for ScriptedSupply {
    async fn ready(&self, region: &str) -> Result<bool, String> {
        self.asked.lock().unwrap().push(region.to_string());
        self.answer.clone()
    }
}

/// No new transcoder on a guess: a supply lookup that fails closes the gate for that tick. The
/// broadcast is still planned (its fan-out is unaffected), and the lookup is about the policy's
/// region.
#[tokio::test]
async fn a_failed_supply_lookup_orders_no_transcoder() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_supply_lookup_orders_no_transcoder");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txerr").await;
    mm_db::transcode_db::set_broadcast_override(
        &pool,
        "txerr",
        "@host-txerr:hs",
        TranscodeOverride::On,
    )
    .await
    .expect("db")
    .expect("host may opt in");
    let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txerr", 0)])),
        Box::new(RichWallet),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    )
    .with_transcode_supply(Box::new(ScriptedSupply {
        answer: Err("database unreachable".into()),
        asked: asked.clone(),
    }));
    let report = runner
        .tick(&DryRunProvider::default(), FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert!(desired_transcoders(&pool).await.is_empty());
    assert_eq!(
        report.planned,
        vec!["txerr".to_string()],
        "{:?}",
        report.skipped
    );
    assert_eq!(
        report.not_promoted,
        vec![("txerr".to_string(), "transcode_software_not_configured")]
    );
    assert_eq!(*asked.lock().unwrap(), vec!["eu-ams".to_string()]);
}

/// The whole FR-314c lifecycle through the real desired store: a transcoder that
/// exists stays desired tick after tick (the flap), an operator release drops it,
/// nothing comes back while it drains or once it is gone, and the broadcaster's
/// explicit re-opt-in orders a FRESH one under a new id.
#[tokio::test]
async fn a_released_transcoder_stays_released_until_the_broadcaster_opts_in_again() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_released_transcoder_stays_released_until_the_broadcaster_opts_in_again");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txrel").await;
    let host = "@host-txrel:hs";
    mm_db::transcode_db::set_broadcast_override(&pool, "txrel", host, TranscodeOverride::On)
        .await
        .expect("db")
        .expect("opt in");

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txrel", 0)])),
        Box::new(RichWallet),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    )
    .with_transcode_supply(Box::new(SupplyReady));
    let provider = DryRunProvider::default();
    let tick = || async { runner.tick(&provider, FleetMode::On, Utc::now()).await.expect("tick") };

    tick().await;
    assert_eq!(desired_transcoders(&pool).await, vec!["bc-txrel-transcode-0"]);

    // The provider created it. Before the fix, THIS tick deleted its desired row.
    insert_transcoder(&pool, "bc-txrel-transcode-0", NodeState::Healthy).await;
    for _ in 0..2 {
        tick().await;
        assert_eq!(
            desired_transcoders(&pool).await,
            vec!["bc-txrel-transcode-0"],
            "a running, wanted transcoder must stay desired"
        );
    }

    // Operator release (P4's audited action sets this flag). The runner destroys
    // the transcoder itself — not by leaving Terraform to notice a missing row,
    // which the tfvars shrink guard refuses for a lone GPU.
    sqlx::query("UPDATE mm_streams SET transcode_released = true WHERE id = 'txrel'")
        .execute(&pool)
        .await
        .expect("release");
    let report = tick().await;
    assert_eq!(report.torn_down, vec!["bc-txrel-transcode-0"]);
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-bc-txrel-transcode-0".into())]
    );
    assert_eq!(node_state(&pool, "bc-txrel-transcode-0").await, "gone");
    assert!(desired_transcoders(&pool).await.is_empty());

    // Sticky: nothing is re-ordered, and nothing is destroyed twice.
    let report = tick().await;
    assert!(report.torn_down.is_empty(), "{:?}", report.torn_down);
    assert_eq!(provider.intents().len(), 1);
    assert!(desired_transcoders(&pool).await.is_empty());

    // Changing the broadcaster default is not opting this broadcast in again.
    mm_db::transcode_db::set_broadcaster_default(&pool, host, true)
        .await
        .expect("default");
    tick().await;
    assert!(desired_transcoders(&pool).await.is_empty());

    mm_db::transcode_db::set_broadcast_override(&pool, "txrel", host, TranscodeOverride::On)
        .await
        .expect("db")
        .expect("re-opt in");
    tick().await;
    assert_eq!(
        desired_transcoders(&pool).await,
        vec!["bc-txrel-transcode-1"],
        "the re-opt-in must order a new transcoder, never re-use the gone one's id"
    );
}

/// A release must stop the spending even when the broadcast's wallet cannot be
/// quoted (no wallet, currency mismatch, unpriced card): the opt-in is read and
/// acted on before billing is asked anything. A transcoder whose destroy already
/// failed (`destroying`) is left to the deadline sweeper rather than retried every
/// tick.
#[tokio::test]
async fn a_release_tears_down_even_when_billing_cannot_be_quoted() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_release_tears_down_even_when_billing_cannot_be_quoted");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    live_stream_row(&pool, "txnoq").await;
    insert_transcoder(&pool, "bc-txnoq-transcode-0", NodeState::Healthy).await;
    insert_transcoder(&pool, "bc-txnoq-transcode-1", NodeState::Destroying).await;
    sqlx::query("UPDATE mm_streams SET transcode_released = true WHERE id = 'txnoq'")
        .execute(&pool)
        .await
        .expect("release");

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("txnoq", 0)])),
        Box::new(NoBillingYet),
        Box::new(PgTranscodeOptIns::new(pool.clone())),
        policy(),
    );
    let provider = DryRunProvider::default();
    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.skipped.len(), 1, "the quote still fails: {:?}", report.skipped);
    assert_eq!(report.torn_down, vec!["bc-txnoq-transcode-0"]);
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-bc-txnoq-transcode-0".into())],
        "the destroying node must be left to the deadline sweeper"
    );
}

// ── a fan-out node whose destroy failed (`destroying`) ───────────────────────

async fn desired_ids(pool: &PgPool) -> Vec<String> {
    let mut ids: Vec<String> = DesiredStore::new(pool.clone())
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

/// `DesiredStore::teardown` deletes a node's desired row BEFORE calling the
/// provider and leaves it deleted when the destroy fails, because a desired row for
/// a machine the provider may have half-destroyed tells Terraform to create a new
/// paid one. Driven through the real failure path — a live broadcast outlives its
/// node's deadline and the deadline sweeper's destroy fails — the next runner tick
/// must not put that row back. The node is no longer capacity, so the shortfall is
/// ordered, under a fresh ordinal.
#[tokio::test]
async fn a_failed_fanout_destroy_is_not_resurrected_by_the_next_tick() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_fanout_destroy_is_not_resurrected_by_the_next_tick");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Healthy).await;
    insert_desired(&pool, "bc-b1-fanout-0", "b1").await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();

    // The node's deadline is three hours out; sweep as if four hours had passed,
    // with the broadcast still on air, and fail the destroy.
    provider.fail_next_destroy(ProviderError::Transient("503".into()));
    let swept = sweep_deadlines(
        &store,
        &provider,
        BillingIncrement::PerHour,
        Utc::now() + Duration::hours(4),
    )
    .await
    .expect("sweep");
    assert_eq!(swept.failed, vec!["bc-b1-fanout-0"]);
    assert_eq!(node_state(&pool, "bc-b1-fanout-0").await, "destroying");
    assert!(
        desired_ids(&pool).await.is_empty(),
        "teardown deletes the desired row before calling the provider"
    );

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 200)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    );
    for tick in 1..=2 {
        let report = runner
            .tick(&provider, FleetMode::On, Utc::now())
            .await
            .expect("tick");
        assert_eq!(report.planned, vec!["b1"], "{:?}", report.skipped);
        assert_eq!(
            desired_ids(&pool).await,
            vec!["bc-b1-fanout-1"],
            "tick {tick}: the destroying node's desired row came back, with a fresh \
             deadline — Terraform would create a paid machine for it"
        );
    }
    assert_eq!(node_state(&pool, "bc-b1-fanout-0").await, "destroying");
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-bc-b1-fanout-0".into())],
        "the runner must not touch the provider for it: the retry is the deadline \
         sweeper's, or `fleet=off`'s"
    );
}

/// The ceiling counts a destroying node, because it may still be billing. With
/// room for one fan-out node and that one stuck, nothing is re-stated and nothing
/// is ordered beside it: two machines must not bill where the ceiling allows one.
#[tokio::test]
async fn a_destroying_fanout_node_still_holds_its_place_under_the_ceiling() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_destroying_fanout_node_still_holds_its_place_under_the_ceiling");
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    // Its desired row is already gone: that is what a failed teardown leaves.
    insert_node(&pool, "bc-b1-fanout-0", Ownership::Rented, NodeState::Destroying).await;

    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[("b1", 200)])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        FleetPolicy {
            max_fanout_nodes_per_broadcast: 1,
            ..policy()
        },
    );
    let provider = DryRunProvider::default();
    let report = runner
        .tick(&provider, FleetMode::On, Utc::now())
        .await
        .expect("tick");

    assert_eq!(report.planned, vec!["b1"], "{:?}", report.skipped);
    assert!(
        desired_ids(&pool).await.is_empty(),
        "neither re-stated nor replaced while the stuck node fills the ceiling"
    );
    assert!(provider.intents().is_empty(), "{:?}", provider.intents());
}

#[tokio::test]
async fn a_deferred_runner_orders_the_teardown_and_calls_no_provider() {
    let Some(pool) = try_pool().await else {
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(
        &pool,
        "bc-gone-fanout-0",
        Ownership::Rented,
        NodeState::Healthy,
    )
    .await;
    insert_desired(&pool, "bc-gone-fanout-0", "gone").await;
    let runner = FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[])), // nothing is live
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    )
    .with_deferred_destroy();
    let provider = DryRunProvider::default();
    let report = runner
        .tick(&provider, FleetMode::Frozen, Utc::now())
        .await
        .expect("tick");
    assert_eq!(report.torn_down, vec!["bc-gone-fanout-0"]);
    assert!(
        provider.intents().is_empty(),
        "mm-core never calls a provider"
    );
    assert_eq!(node_state(&pool, "bc-gone-fanout-0").await, "destroying");
    assert!(
        DesiredStore::new(pool.clone())
            .load_all()
            .await
            .unwrap()
            .is_empty()
    );
}

fn runner_over(pool: &PgPool) -> FleetRunner {
    FleetRunner::new(
        DesiredStore::new(pool.clone()),
        Box::new(FakeCensus::with(&[])),
        Box::new(RichWallet),
        Box::new(NobodyOptedIn),
        policy(),
    )
}

#[tokio::test]
async fn a_deferred_off_tick_orders_a_node_once_and_skips_one_already_destroying() {
    let Some(pool) = try_pool().await else {
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "n-live", Ownership::Rented, NodeState::Healthy).await;
    insert_node(&pool, "n-dying", Ownership::Rented, NodeState::Destroying).await;
    let runner = runner_over(&pool).with_deferred_destroy();
    let provider = DryRunProvider::default();
    let report = runner
        .tick(&provider, FleetMode::Off, Utc::now())
        .await
        .expect("tick");
    assert_eq!(
        report.torn_down,
        vec!["n-live"],
        "an order already placed is not placed again every tick"
    );
    assert!(report.teardown_failures.is_empty());
    assert!(provider.intents().is_empty());
    assert_eq!(node_state(&pool, "n-live").await, "destroying");
    assert_eq!(node_state(&pool, "n-dying").await, "destroying");
}

#[tokio::test]
async fn an_off_tick_that_destroys_itself_still_retries_a_destroying_node() {
    let Some(pool) = try_pool().await else {
        return;
    };
    let _guard = runner_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_node(&pool, "n-dying", Ownership::Rented, NodeState::Destroying).await;
    let provider = DryRunProvider::default();
    let report = runner_over(&pool)
        .tick(&provider, FleetMode::Off, Utc::now())
        .await
        .expect("tick");
    assert_eq!(report.torn_down, vec!["n-dying"]);
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-n-dying".into())]
    );
    assert_eq!(node_state(&pool, "n-dying").await, "gone");
}
