//! PG-gated coverage for the two sweep LOOPS (WS-B Tasks B3, B4).
//!
//! The selectors are unit-tested in the module. What needs a database is the
//! behaviour around them: that a sweep destroys what it selected, that one
//! failure does not abandon the rest of an over-running fleet, and — the one that
//! matters most — that a provider listing failure destroys **nothing**.

use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_core::fleet::billing::BillingIncrement;
use mm_core::fleet::{NodeId, NodeState, Ownership};
use mm_fleet::desired::DesiredStore;
use mm_fleet::provider::{DryRunProvider, Intent, ProviderError};
use mm_fleet::sweeper::{sweep_deadlines, sweep_deadlines_skipping, sweep_orphans};
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;

/// The production default. `seed()` instances are long dead, so it changes
/// nothing for the tests written before the grace existed.
const GRACE: Duration = Duration::minutes(30);

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

fn sweep_lock() -> &'static AsyncMutex<()> {
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

/// Inserts a node row with a deadline `offset` from now (negative = overdue).
async fn insert_node(
    pool: &PgPool,
    id: &str,
    ownership: Ownership,
    offset: Duration,
    state: NodeState,
) {
    let deadline = ownership
        .requires_destroy_deadline()
        .then(|| Utc::now() + offset);
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

async fn node_state(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read node state")
}

// ─── B3: the deadline sweep ──────────────────────────────────────────────────

#[tokio::test]
async fn an_overdue_rented_node_is_destroyed_and_an_early_one_is_not() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_overdue_rented_node_is_destroyed_and_an_early_one_is_not");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "late", Ownership::Rented, Duration::minutes(-5), NodeState::Healthy).await;
    insert_node(&pool, "early", Ownership::Rented, Duration::hours(2), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report.reaped, vec!["late"]);
    assert!(report.failed.is_empty());
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-late".into())],
        "only the overdue node may be touched"
    );
    assert_eq!(node_state(&pool, "late").await, NodeState::Gone.as_str());
    assert_eq!(node_state(&pool, "early").await, NodeState::Healthy.as_str());
}

/// The caller that can look a handle-less row up by its tag keeps it out of the sweep: `teardown`
/// would mark it `gone` with no provider call. Everything else overdue is reaped as ever, and a
/// skipped node that is not overdue is not an error.
#[tokio::test]
async fn a_node_the_caller_asks_to_skip_is_left_alone_and_the_rest_are_reaped() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_node_the_caller_asks_to_skip_is_left_alone_and_the_rest_are_reaped"
        );
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(
        &pool,
        "a-skipped",
        Ownership::Rented,
        Duration::minutes(-5),
        NodeState::Healthy,
    )
    .await;
    insert_node(
        &pool,
        "b-reaped",
        Ownership::Rented,
        Duration::minutes(-5),
        NodeState::Healthy,
    )
    .await;
    insert_node(
        &pool,
        "c-early",
        Ownership::Rented,
        Duration::hours(2),
        NodeState::Healthy,
    )
    .await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let skip: std::collections::HashSet<NodeId> = ["a-skipped", "c-early"]
        .into_iter()
        .map(NodeId::new)
        .collect();
    let report = sweep_deadlines_skipping(
        &store,
        &provider,
        BillingIncrement::PerHour,
        Utc::now(),
        &skip,
    )
    .await
    .expect("sweep");

    assert_eq!(report.reaped, vec!["b-reaped"]);
    assert_eq!(
        provider.intents(),
        vec![Intent::Destroy("prov-b-reaped".into())]
    );
    assert_eq!(
        node_state(&pool, "a-skipped").await,
        NodeState::Healthy.as_str()
    );
    assert_eq!(
        node_state(&pool, "b-reaped").await,
        NodeState::Gone.as_str()
    );
}

/// The guard, end to end through the database. An owned node with a 1970 deadline
/// should be impossible, but if a bug writes one the sweeper must not destroy
/// colocated hardware.
#[tokio::test]
async fn the_sweep_never_touches_owned_or_leased_hardware() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_sweep_never_touches_owned_or_leased_hardware");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    // The CHECK forbids a deadline on non-rented nodes, so the only way such a
    // node reaches the sweeper is with NO deadline — which must also be ignored.
    insert_node(&pool, "owned", Ownership::Owned, Duration::hours(-100), NodeState::Healthy).await;
    insert_node(&pool, "leased", Ownership::Leased, Duration::hours(-100), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert!(report.reaped.is_empty(), "reaped: {:?}", report.reaped);
    assert!(
        provider.intents().is_empty(),
        "the provider was called for non-reapable hardware: {:?}",
        provider.intents()
    );
    assert_eq!(node_state(&pool, "owned").await, NodeState::Healthy.as_str());
    assert_eq!(node_state(&pool, "leased").await, NodeState::Healthy.as_str());
}

/// One provider failure must not abandon the rest of an over-running fleet. Each
/// node is independent and every one of them is billing.
#[tokio::test]
async fn one_failure_does_not_abort_the_sweep() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping one_failure_does_not_abort_the_sweep");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    for id in ["a", "b", "c"] {
        insert_node(&pool, id, Ownership::Rented, Duration::minutes(-1), NodeState::Healthy).await;
    }

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    // load_nodes orders by id, so "a" is attempted first and fails.
    provider.fail_next_destroy(ProviderError::Transient("503".into()));

    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("a provider failure is reported in the sweep, not returned as Err");

    assert_eq!(report.failed, vec!["a"]);
    assert_eq!(report.reaped, vec!["b", "c"], "the rest must still be reaped");
    assert_eq!(
        node_state(&pool, "a").await,
        NodeState::Destroying.as_str(),
        "the failed node must stay findable by the orphan sweeper"
    );
}

#[tokio::test]
async fn a_sweep_with_nothing_due_does_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_sweep_with_nothing_due_does_nothing");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "fine", Ownership::Rented, Duration::hours(3), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report, Default::default());
    assert!(provider.intents().is_empty());
}

// ─── B4: the orphan sweep ────────────────────────────────────────────────────

#[tokio::test]
async fn an_instance_with_no_node_row_is_destroyed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_instance_with_no_node_row_is_destroyed");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "ours", Ownership::Rented, Duration::hours(2), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    provider.seed(&["prov-ours", "prov-forgotten"]);

    let report = sweep_orphans(&store, &provider, Utc::now(), GRACE)
        .await
        .expect("sweep");

    assert_eq!(report.reaped, vec!["prov-forgotten"]);
    assert!(
        provider.live().iter().any(|h| h.provider_id == "prov-ours"),
        "a machine we have a row for must survive"
    );
}

/// THE ONE THAT COULD DESTROY THE FLEET.
///
/// An empty provider listing is indistinguishable from "every instance we know
/// about is an orphan". Treating a listing error as an empty list — or defaulting
/// to Vec::new() on Err — destroys everything on a provider 503.
#[tokio::test]
async fn a_provider_listing_failure_destroys_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_provider_listing_failure_destroys_nothing");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    provider.seed(&["vm-1", "vm-2", "vm-3"]);
    provider.fail_next_list(ProviderError::Transient("503".into()));

    let err = sweep_orphans(&store, &provider, Utc::now(), GRACE)
        .await
        .expect_err("a listing failure must surface, not be read as an empty fleet");
    assert!(err.is_transient());

    assert_eq!(
        provider.live().len(),
        3,
        "a listing failure destroyed instances — an Err was treated as an empty list"
    );
    assert!(
        !provider
            .intents()
            .iter()
            .any(|i| matches!(i, Intent::Destroy(_))),
        "nothing may be destroyed when we cannot see what exists: {:?}",
        provider.intents()
    );
}

/// A `gone` row still counts as a node we know about. Its machine should not
/// exist, but if it does, reaping it as an orphan would be right by accident —
/// and the same reasoning would reap a node mid-create whose state has not caught
/// up with its row.
#[tokio::test]
async fn a_gone_nodes_instance_is_not_treated_as_an_orphan() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_gone_nodes_instance_is_not_treated_as_an_orphan");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "closed", Ownership::Rented, Duration::hours(1), NodeState::Gone).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    provider.seed(&["prov-closed"]);

    let report = sweep_orphans(&store, &provider, Utc::now(), GRACE)
        .await
        .expect("sweep");
    assert!(
        report.reaped.is_empty(),
        "a row we still hold — in any state — is not an orphan: {:?}",
        report.reaped
    );
}

#[tokio::test]
async fn an_orphan_sweep_with_an_empty_provider_does_nothing() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_orphan_sweep_with_an_empty_provider_does_nothing");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "ours", Ownership::Rented, Duration::hours(2), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_orphans(&store, &provider, Utc::now(), GRACE)
        .await
        .expect("sweep");
    assert_eq!(report, Default::default());
}

// ── Billing-hour alignment (§B.0: Scaleway CPU Instances bill per hour) ───────

/// Inserts an overdue node with explicit control of both clocks, because the two
/// are independent and the alignment logic depends on their relationship:
/// `started_mins_ago` sets the billing-period phase, `deadline_mins_ago` sets how
/// long the node has been overdue.
async fn insert_overdue_node(
    pool: &PgPool,
    id: &str,
    started_mins_ago: i64,
    deadline_mins_ago: i64,
) {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, provider_id, state,
              destroy_deadline, billing_started_at, viewer_capacity, viewers_current)
         VALUES ($1, 'fanout', 'rented', 'scaleway', $2, 'healthy', $3, $4, 640, 100)",
    )
    .bind(id)
    .bind(format!("prov-{id}"))
    .bind(now - Duration::minutes(deadline_mins_ago))
    .bind(now - Duration::minutes(started_mins_ago))
    .execute(pool)
    .await
    .expect("insert node");
}

/// THE ONE THAT SAVES MONEY. An overdue node five minutes into a paid hour has 55
/// minutes of already-purchased service left; destroying now would refund nothing
/// and drop 100 viewers.
#[tokio::test]
async fn an_overdue_node_early_in_a_paid_hour_is_deferred_not_destroyed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_overdue_node_early_in_a_paid_hour_is_deferred_not_destroyed");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_overdue_node(&pool, "midhour", 65, 1).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report.deferred, vec!["midhour"]);
    assert!(report.reaped.is_empty());
    assert!(
        provider.intents().is_empty(),
        "the provider must not be called: the hour is already paid for"
    );
    assert_eq!(
        node_state(&pool, "midhour").await,
        NodeState::Healthy.as_str(),
        "a deferred node keeps serving its viewers"
    );
}

/// Once the deadline's own billing period has ended there is nothing left to save,
/// so the node goes. This is the case that stops deferral being a reprieve.
#[tokio::test]
async fn an_overdue_node_whose_paid_period_already_ended_is_destroyed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_overdue_node_at_the_end_of_its_paid_hour_is_destroyed");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    // Started 130m ago, deadline 70m ago: the deadline fell 60m into the billing
    // clock, so its period ended at started+120m = 10 minutes ago.
    insert_overdue_node(&pool, "boundary", 130, 70).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report.reaped, vec!["boundary"]);
    assert!(report.deferred.is_empty());
    assert_eq!(node_state(&pool, "boundary").await, NodeState::Gone.as_str());
}

/// Per-minute billing never defers. Deferring to save 59 seconds would trade a
/// cost-safety action for a rounding error — and GPU transcode nodes bill per
/// minute, so this is the live case, not a hypothetical.
#[tokio::test]
async fn a_per_minute_node_is_destroyed_immediately_however_far_into_its_minute() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_per_minute_node_is_destroyed_immediately_however_far_into_its_minute");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_overdue_node(&pool, "gpu", 65, 1).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerMinute, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report.reaped, vec!["gpu"], "per-minute billing must not defer");
    assert!(report.deferred.is_empty());
}

/// A node with no billing start has no boundary to compute, so it goes now. The
/// safe direction for a cost-safety mechanism is to act, not to wait.
#[tokio::test]
async fn a_node_with_no_billing_start_is_destroyed_immediately() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_node_with_no_billing_start_is_destroyed_immediately");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    // insert_node leaves billing_started_at NULL.
    insert_node(&pool, "nostart", Ownership::Rented, Duration::minutes(-5), NodeState::Healthy).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let report = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, Utc::now())
        .await
        .expect("sweep");

    assert_eq!(report.reaped, vec!["nostart"]);
    assert!(report.deferred.is_empty());
}

/// Deferral must never become indefinite: a deferred node whose boundary has since
/// passed is destroyed on the next sweep. Otherwise the alignment optimisation
/// would have turned the cost backstop off.
#[tokio::test]
async fn a_deferred_node_is_destroyed_once_its_boundary_passes() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_deferred_node_is_destroyed_once_its_boundary_passes");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;
    insert_overdue_node(&pool, "later", 65, 1).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();

    // Now: deferred.
    let now = Utc::now();
    let first = sweep_deadlines(&store, &provider, BillingIncrement::PerHour, now)
        .await
        .expect("sweep");
    assert_eq!(first.deferred, vec!["later"]);

    // 56 minutes later the boundary has passed.
    let second = sweep_deadlines(
        &store,
        &provider,
        BillingIncrement::PerHour,
        now + Duration::minutes(56),
    )
    .await
    .expect("sweep");
    assert_eq!(
        second.reaped,
        vec!["later"],
        "deferral must be a delay, not a reprieve — otherwise alignment turned the \
         cost backstop off"
    );
}

// ── The orphan grace: a node mid-create has no row yet ────────────────────────

/// THE RACE THE GRACE CLOSES. A create in flight has a machine at the provider
/// and no row in our table yet; without a minimum age, the sweeper reads that as
/// "a machine we forgot" and destroys a live broadcast's node.
#[tokio::test]
async fn a_young_instance_with_no_node_row_is_spared() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_young_instance_with_no_node_row_is_spared");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let now = Utc::now();
    provider.seed_created_at("prov-new", Some(now - Duration::minutes(5)));

    let report = sweep_orphans(&store, &provider, now, GRACE).await.expect("sweep");

    assert!(report.reaped.is_empty(), "{:?}", report.reaped);
    assert_eq!(report.spared, vec!["prov-new"]);
    assert!(
        !provider.intents().iter().any(|i| matches!(i, Intent::Destroy(_))),
        "{:?}",
        provider.intents()
    );
}

#[tokio::test]
async fn an_unknown_instance_of_unknown_age_is_spared() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_unknown_instance_of_unknown_age_is_spared");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    provider.seed_created_at("prov-ageless", None);

    let report = sweep_orphans(&store, &provider, Utc::now(), GRACE).await.expect("sweep");
    assert!(report.reaped.is_empty());
    assert_eq!(report.spared, vec!["prov-ageless"]);
}

/// The grace delays an orphan; it never pardons one.
#[tokio::test]
async fn once_past_the_grace_an_orphan_is_destroyed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping once_past_the_grace_an_orphan_is_destroyed");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let now = Utc::now();
    provider.seed_created_at("prov-old", Some(now - Duration::minutes(31)));

    let report = sweep_orphans(&store, &provider, now, GRACE).await.expect("sweep");
    assert_eq!(report.reaped, vec!["prov-old"]);
    assert!(report.spared.is_empty());
}

/// `spared` lists would-be orphans only. A young machine we DO have a row for is
/// simply ours, and listing it would make the report noise.
#[tokio::test]
async fn a_young_instance_we_know_is_not_reported_as_spared() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_young_instance_we_know_is_not_reported_as_spared");
        return;
    };
    let _guard = sweep_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    insert_node(&pool, "ours", Ownership::Rented, Duration::hours(2), NodeState::Healthy).await;
    let store = DesiredStore::new(pool.clone());
    let provider = DryRunProvider::default();
    let now = Utc::now();
    provider.seed_created_at("prov-ours", Some(now - Duration::minutes(1)));

    let report = sweep_orphans(&store, &provider, now, GRACE).await.expect("sweep");
    assert_eq!(report, Default::default());
}
