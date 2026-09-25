//! PG-gated coverage for the two sweep LOOPS (WS-B Tasks B3, B4).
//!
//! The selectors are unit-tested in the module. What needs a database is the
//! behaviour around them: that a sweep destroys what it selected, that one
//! failure does not abandon the rest of an over-running fleet, and — the one that
//! matters most — that a provider listing failure destroys **nothing**.

use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_core::fleet::{NodeState, Ownership};
use mm_fleet::desired::DesiredStore;
use mm_fleet::provider::{DryRunProvider, Intent, Provider, ProviderError};
use mm_fleet::sweeper::{sweep_deadlines, sweep_orphans};
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
    let report = sweep_deadlines(&store, &provider, Utc::now())
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
    let report = sweep_deadlines(&store, &provider, Utc::now())
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

    let report = sweep_deadlines(&store, &provider, Utc::now())
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
    let report = sweep_deadlines(&store, &provider, Utc::now())
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

    let report = sweep_orphans(&store, &provider, Utc::now())
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

    let err = sweep_orphans(&store, &provider, Utc::now())
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

    let report = sweep_orphans(&store, &provider, Utc::now())
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
    let report = sweep_orphans(&store, &provider, Utc::now())
        .await
        .expect("sweep");
    assert_eq!(report, Default::default());
}
