//! PG-gated coverage for the desired-set store (WS-B Task B2).
//!
//! The CRUD is not what these test. Both cost-safety invariants are about
//! **sequence**, and a test that only inspects the end state passes for either
//! ordering — so the interesting cases here make the provider observe the
//! database at the moment it is called, which is the only way to assert that the
//! desired row was already gone.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use mm_core::fleet::planner::DesiredNode;
use mm_core::fleet::{NodeFlavor, NodeId, NodeState, Ownership};
use mm_fleet::desired::{DesiredStore, StoreError, TeardownTarget, DESIRED_WRITE_LOCK};
use mm_fleet::provider::{InstanceHandle, InstanceSpec, Provider, ProviderError};
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

fn fleet_lock() -> &'static AsyncMutex<()> {
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

async fn insert_node(pool: &PgPool, id: &str, ownership: Ownership, provider_id: &str) {
    let deadline = ownership
        .requires_destroy_deadline()
        .then(|| Utc::now() + Duration::hours(1));
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, provider_id, state, destroy_deadline, renewal_due_at)
         VALUES ($1, 'fanout', $2, 'dry-run', $3, 'healthy', $4, $5)",
    )
    .bind(id)
    .bind(ownership.as_str())
    .bind(provider_id)
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

fn fanout(id: &str, ownership: Ownership, ttl: Option<u32>) -> DesiredNode {
    DesiredNode {
        mm_node_id: NodeId::new(id),
        flavor: NodeFlavor::Fanout,
        ownership,
        region: "eu-ams".into(),
        size: "small".into(),
        broadcast_id: "b1".into(),
        destroy_after_secs: ttl,
    }
}

/// A provider that reads the database at the instant `destroy` is called, so the
/// ORDER can be asserted rather than inferred.
struct ObservingProvider {
    pool: PgPool,
    /// Whether the desired row still existed when destroy ran.
    row_present_at_destroy: Arc<Mutex<Option<bool>>>,
    fail: Option<ProviderError>,
}

#[async_trait]
impl Provider for ObservingProvider {
    fn name(&self) -> &'static str {
        "observing"
    }

    async fn create(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        unimplemented!("teardown tests never create")
    }

    async fn destroy(&self, _provider_id: &str) -> Result<(), ProviderError> {
        let present: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
            .fetch_one(&self.pool)
            .await
            .expect("count desired rows from inside destroy");
        *self.row_present_at_destroy.lock().unwrap() = Some(present > 0);

        match &self.fail {
            Some(ProviderError::Transient(m)) => Err(ProviderError::Transient(m.clone())),
            Some(ProviderError::Permanent(m)) => Err(ProviderError::Permanent(m.clone())),
            Some(ProviderError::Capacity(m)) => Err(ProviderError::Capacity(m.clone())),
            Some(ProviderError::Quota(m)) => Err(ProviderError::Quota(m.clone())),
            None => Ok(()),
        }
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
}

// ── The invariant that made destroy_deadline move tables in WS-A ─────────────

#[tokio::test]
async fn a_rented_desired_row_cannot_be_written_without_a_deadline() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_rented_desired_row_cannot_be_written_without_a_deadline");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let err = store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, None)], Utc::now())
        .await
        .expect_err("a rented node with no TTL must be refused");

    assert!(
        matches!(err, StoreError::RentedWithoutDeadline { .. }),
        "expected a typed refusal so the caller can tell a bug from an outage, got: {err}"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "nothing may be written when the batch is refused");
}

#[tokio::test]
async fn an_owned_desired_row_cannot_carry_a_ttl() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_owned_desired_row_cannot_carry_a_ttl");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let err = store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Owned, Some(3600))], Utc::now())
        .await
        .expect_err("a deadline on owned hardware must be refused");
    assert!(matches!(err, StoreError::NonRentedWithDeadline { .. }), "got {err}");
}

/// A half-legal batch is a planner bug, and finding out halfway through means
/// deciding whether to keep the legal half — which is never right.
#[tokio::test]
async fn one_bad_node_rejects_the_whole_batch() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping one_bad_node_rejects_the_whole_batch");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let batch = [
        fanout("good", Ownership::Rented, Some(3600)),
        fanout("bad", Ownership::Rented, None),
    ];
    store
        .upsert_for_broadcast("b1", &batch, Utc::now())
        .await
        .expect_err("must refuse");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "the legal half must not be written either");
}

#[tokio::test]
async fn the_ttl_becomes_a_deadline_relative_to_the_supplied_instant() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_ttl_becomes_a_deadline_relative_to_the_supplied_instant");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let now = Utc::now();
    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(7200))], now)
        .await
        .expect("upsert");

    let rows = store.load_all().await.expect("load");
    assert_eq!(rows.len(), 1);
    let deadline = rows[0].destroy_deadline.expect("a rented row must have one");
    let drift = (deadline - (now + Duration::seconds(7200)))
        .num_milliseconds()
        .abs();
    assert!(drift < 1_000, "deadline drifted {drift}ms from now + TTL");
}

#[tokio::test]
async fn a_shrinking_desired_set_actually_shrinks() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_shrinking_desired_set_actually_shrinks");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let now = Utc::now();
    store
        .upsert_for_broadcast(
            "b1",
            &[
                fanout("n0", Ownership::Rented, Some(3600)),
                fanout("n1", Ownership::Rented, Some(3600)),
            ],
            now,
        )
        .await
        .expect("upsert two");

    store
        .upsert_for_broadcast("b1", &[fanout("n0", Ownership::Rented, Some(3600))], now)
        .await
        .expect("upsert one");

    let ids: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert_eq!(ids, vec!["n0"], "a node no longer desired must be removed");
}

/// The WS-A BLOCKER, now asserted through the store rather than the schema:
/// removing the LAST desired row must still be visible to the runner.
#[tokio::test]
async fn removing_the_last_row_bumps_the_generation() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping removing_the_last_row_bumps_the_generation");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");

    let before = store.generation().await.expect("generation");
    store
        .upsert_for_broadcast("b1", &[], Utc::now())
        .await
        .expect("empty upsert");
    let after = store.generation().await.expect("generation");

    assert!(
        after > before,
        "emptying the desired set must be visible ({before} → {after}); a \
         per-row counter made teardown silently do nothing"
    );
    assert!(store.load_all().await.expect("load").is_empty());
}

// ── The teardown ordering invariant ─────────────────────────────────────────

/// THE ONE THAT MATTERS.
///
/// If the desired row is still present when the provider destroys the instance,
/// then a failure to delete it afterwards leaves Terraform wanting a node that no
/// longer exists — and the next apply CREATES A NEW PAID MACHINE. Teardown would
/// have become provisioning.
#[tokio::test]
async fn teardown_removes_the_desired_row_before_it_destroys() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping teardown_removes_the_desired_row_before_it_destroys");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "n1", Ownership::Rented, "prov-n1").await;

    let observed = Arc::new(Mutex::new(None));
    let provider = ObservingProvider {
        pool: pool.clone(),
        row_present_at_destroy: observed.clone(),
        fail: None,
    };

    store
        .teardown(
            &provider,
            &TeardownTarget {
                mm_node_id: NodeId::new("n1"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Fanout,
                provider_id: Some("prov-n1".into()),
            },
        )
        .await
        .expect("teardown");

    assert_eq!(
        *observed.lock().unwrap(),
        Some(false),
        "the desired row was STILL PRESENT when the provider was called — reverse \
         the order, or a failed deletion turns the next apply into a new paid machine"
    );
    assert_eq!(node_state(&pool, "n1").await, NodeState::Gone.as_str());
}

/// On provider failure the row stays deleted. Resurrecting it would reintroduce
/// exactly the creation the ordering exists to prevent; the orphan sweeper is the
/// correct backstop.
#[tokio::test]
async fn a_failed_destroy_leaves_the_row_deleted_and_the_node_marked_destroying() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_failed_destroy_leaves_the_row_deleted_and_the_node_marked_destroying");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "n1", Ownership::Rented, "prov-n1").await;

    let provider = ObservingProvider {
        pool: pool.clone(),
        row_present_at_destroy: Arc::new(Mutex::new(None)),
        fail: Some(ProviderError::Transient("503".into())),
    };

    let err = store
        .teardown(
            &provider,
            &TeardownTarget {
                mm_node_id: NodeId::new("n1"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Fanout,
                provider_id: Some("prov-n1".into()),
            },
        )
        .await
        .expect_err("the provider failed, so teardown must report it");
    assert!(matches!(err, StoreError::Provider { .. }), "got {err}");

    assert!(
        store.load_all().await.expect("load").is_empty(),
        "the desired row must STAY deleted — resurrecting it makes the next apply \
         create a replacement for the machine we just failed to destroy"
    );
    assert_eq!(
        node_state(&pool, "n1").await,
        NodeState::Destroying.as_str(),
        "the node must be left findable by the orphan sweeper"
    );
}

/// `Ownership::is_reapable` must be IN THE PATH, not in a comment. This is the
/// test that proves it.
#[tokio::test]
async fn teardown_refuses_owned_and_leased_nodes() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping teardown_refuses_owned_and_leased_nodes");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());

    for ownership in [Ownership::Owned, Ownership::Leased] {
        insert_node(&pool, "protected", ownership, "prov-protected").await;
        let observed = Arc::new(Mutex::new(None));
        let provider = ObservingProvider {
            pool: pool.clone(),
            row_present_at_destroy: observed.clone(),
            fail: None,
        };

        let err = store
            .teardown(
                &provider,
                &TeardownTarget {
                    mm_node_id: NodeId::new("protected"),
                    ownership,
                    flavor: NodeFlavor::Fanout,
                    provider_id: Some("prov-protected".into()),
                },
            )
            .await
            .expect_err("non-reapable ownership must be refused");
        assert!(matches!(err, StoreError::NotReapable { .. }), "got {err}");

        assert_eq!(
            node_state(&pool, "protected").await,
            "healthy",
            "a refused teardown must not touch the node's state either"
        );
        assert!(
            observed.lock().unwrap().is_none(),
            "the provider must not have been called at all for {ownership}"
        );
        wipe(&pool).await;
    }
}

/// A node with no provider handle may still exist at the provider — the create
/// call may have succeeded and failed to tell us. Teardown has nothing to call,
/// so it closes the books and leaves the machine to the orphan sweeper.
#[tokio::test]
async fn a_node_with_no_provider_handle_is_closed_without_a_provider_call() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_node_with_no_provider_handle_is_closed_without_a_provider_call");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    insert_node(&pool, "n1", Ownership::Rented, "").await;
    let observed = Arc::new(Mutex::new(None));
    let provider = ObservingProvider {
        pool: pool.clone(),
        row_present_at_destroy: observed.clone(),
        fail: Some(ProviderError::Permanent("must not be called".into())),
    };

    store
        .teardown(
            &provider,
            &TeardownTarget {
                mm_node_id: NodeId::new("n1"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Fanout,
                provider_id: None,
            },
        )
        .await
        .expect("no handle means nothing to destroy, not a failure");

    assert!(observed.lock().unwrap().is_none(), "provider must not be called");
    assert_eq!(node_state(&pool, "n1").await, NodeState::Gone.as_str());
}

// ── The shrink guard's evidence ──────────────────────────────────────────────

/// `torn_down()` is what lets the tfvars shrink guard tell a teardown landing from
/// a partial read, so it must name exactly what teardown leaves behind — a destroy
/// that succeeded (`gone`) AND one that failed (`destroying`: its desired row is
/// deleted all the same) — and nothing teardown cannot have produced: a node still
/// serving or draining, or a non-reapable one in either state.
#[tokio::test]
async fn torn_down_names_exactly_what_teardown_left_behind() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping torn_down_names_exactly_what_teardown_left_behind");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast(
            "b1",
            &[
                fanout("destroyed", Ownership::Rented, Some(3600)),
                fanout("destroy-failed", Ownership::Rented, Some(3600)),
                fanout("serving", Ownership::Rented, Some(3600)),
            ],
            Utc::now(),
        )
        .await
        .expect("upsert");
    for id in ["destroyed", "destroy-failed", "serving", "draining"] {
        insert_node(&pool, id, Ownership::Rented, &format!("prov-{id}")).await;
    }
    insert_node(&pool, "owned-gone", Ownership::Owned, "prov-owned-gone").await;
    for (id, state) in [("draining", NodeState::Draining), ("owned-gone", NodeState::Gone)] {
        sqlx::query("UPDATE mm_fleet_nodes SET state = $2 WHERE mm_node_id = $1")
            .bind(id)
            .bind(state.as_str())
            .execute(&pool)
            .await
            .expect("set state");
    }

    let target = |id: &str| TeardownTarget {
        mm_node_id: NodeId::new(id),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Fanout,
        provider_id: Some(format!("prov-{id}")),
    };
    let provider = |fail: Option<ProviderError>| ObservingProvider {
        pool: pool.clone(),
        row_present_at_destroy: Arc::new(Mutex::new(None)),
        fail,
    };
    store
        .teardown(&provider(None), &target("destroyed"))
        .await
        .expect("teardown");
    store
        .teardown(
            &provider(Some(ProviderError::Transient("503".into()))),
            &target("destroy-failed"),
        )
        .await
        .expect_err("the provider failed");

    let mut got: Vec<String> = store
        .torn_down()
        .await
        .expect("read")
        .into_iter()
        .map(|n| n.as_str().to_string())
        .collect();
    got.sort();
    assert_eq!(got, vec!["destroy-failed", "destroyed"]);
}

// ── The intent is recorded before the irreversible call ──────────────────────

/// What the provider can see of one node at the instant it is asked to destroy it.
struct StateAtDestroy {
    pool: PgPool,
    node: &'static str,
    /// (desired rows for the node, the node's state), read inside `destroy`.
    seen: Arc<Mutex<Option<(i64, String)>>>,
}

#[async_trait]
impl Provider for StateAtDestroy {
    fn name(&self) -> &'static str {
        "state-at-destroy"
    }

    async fn create(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        unimplemented!("teardown tests never create")
    }

    async fn destroy(&self, _provider_id: &str) -> Result<(), ProviderError> {
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE mm_node_id = $1")
            .bind(self.node)
            .fetch_one(&self.pool)
            .await
            .expect("count the node's desired rows from inside destroy");
        let state = node_state(&self.pool, self.node).await;
        *self.seen.lock().unwrap() = Some((rows, state));
        Ok(())
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
}

/// By the time the provider is called, the node must already say `destroying`.
///
/// Written only after the call returns, the outcome is lost whenever the process
/// dies inside it — or the write fails after a successful destroy — and the node
/// is left with no desired row in a state that looks alive: the planner re-states
/// it (a desired row for a machine that may already be gone, which the next apply
/// creates), and the tfvars shrink guard has no evidence its removal was a
/// teardown.
#[tokio::test]
async fn the_node_is_already_destroying_when_the_provider_is_called() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_node_is_already_destroying_when_the_provider_is_called");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "n1", Ownership::Rented, "prov-n1").await;

    let seen = Arc::new(Mutex::new(None));
    store
        .teardown(
            &StateAtDestroy {
                pool: pool.clone(),
                node: "n1",
                seen: seen.clone(),
            },
            &TeardownTarget {
                mm_node_id: NodeId::new("n1"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Fanout,
                provider_id: Some("prov-n1".into()),
            },
        )
        .await
        .expect("teardown");

    assert_eq!(
        *seen.lock().unwrap(),
        Some((0, NodeState::Destroying.as_str().to_string())),
        "at the provider call the desired row must be gone AND the node already \
         marked destroying"
    );
    assert_eq!(
        node_state(&pool, "n1").await,
        NodeState::Gone.as_str(),
        "a destroy that returned Ok still ends gone"
    );
}

/// Step 2 is ONE transaction. If the mark cannot be written, the desired row must
/// still be there and the provider must not have been called: a deleted row
/// without the mark is exactly the state the mark exists to prevent.
///
/// The failure is injected with a trigger that refuses the mark for this test's
/// node id only, so suites sharing the database are unaffected.
#[tokio::test]
async fn if_the_destroying_mark_cannot_be_written_the_desired_row_is_not_deleted_either() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping if_the_destroying_mark_cannot_be_written_the_desired_row_is_not_deleted_either");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("atomic-probe", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "atomic-probe", Ownership::Rented, "prov-atomic-probe").await;

    for ddl in [
        "CREATE OR REPLACE FUNCTION mm_test_refuse_destroying_mark() RETURNS trigger AS $$
         BEGIN
             IF NEW.mm_node_id = 'atomic-probe' AND NEW.state = 'destroying' THEN
                 RAISE EXCEPTION 'test: refusing the destroying mark';
             END IF;
             RETURN NEW;
         END $$ LANGUAGE plpgsql",
        "DROP TRIGGER IF EXISTS mm_test_refuse_destroying_mark ON mm_fleet_nodes",
        "CREATE TRIGGER mm_test_refuse_destroying_mark BEFORE UPDATE ON mm_fleet_nodes
         FOR EACH ROW EXECUTE FUNCTION mm_test_refuse_destroying_mark()",
    ] {
        sqlx::query(ddl).execute(&pool).await.expect("install the refusing trigger");
    }

    let observed = Arc::new(Mutex::new(None));
    let result = store
        .teardown(
            &ObservingProvider {
                pool: pool.clone(),
                row_present_at_destroy: observed.clone(),
                fail: None,
            },
            &TeardownTarget {
                mm_node_id: NodeId::new("atomic-probe"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Fanout,
                provider_id: Some("prov-atomic-probe".into()),
            },
        )
        .await;

    // Removed before any assertion, so a failure cannot leave it behind.
    sqlx::query("DROP TRIGGER IF EXISTS mm_test_refuse_destroying_mark ON mm_fleet_nodes")
        .execute(&pool)
        .await
        .expect("drop the trigger");

    let err = result.expect_err("the mark failed, so teardown must fail");
    assert!(matches!(err, StoreError::Db(_)), "got {err}");
    let ids: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["atomic-probe"],
        "the desired row was deleted without the mark — step 2 is not one transaction"
    );
    assert!(observed.lock().unwrap().is_none(), "the provider must not have been called");
    assert_eq!(node_state(&pool, "atomic-probe").await, "healthy");
}

// ── A torn-down node is never desired again ──────────────────────────────────

fn rented(id: &str) -> TeardownTarget {
    TeardownTarget {
        mm_node_id: NodeId::new(id),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Fanout,
        provider_id: Some(format!("prov-{id}")),
    }
}

async fn desired_ids(store: &DesiredStore) -> Vec<String> {
    let mut ids: Vec<String> = store
        .load_all()
        .await
        .expect("load")
        .into_iter()
        .map(|r| r.mm_node_id.as_str().to_string())
        .collect();
    ids.sort();
    ids
}

/// A desired row for a node teardown has acted on is a machine Terraform creates.
/// `upsert_for_broadcast` is the only writer that inserts desired rows, so it
/// refuses them whatever the plan says: a plan built from a node snapshot taken
/// before the teardown still re-states the node as if nothing had happened.
#[tokio::test]
async fn an_upsert_never_restates_a_node_teardown_has_acted_on() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_upsert_never_restates_a_node_teardown_has_acted_on");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    let planned: Vec<DesiredNode> = ["destroyed", "destroy-failed", "serving"]
        .iter()
        .map(|id| fanout(id, Ownership::Rented, Some(3600)))
        .collect();
    store.upsert_for_broadcast("b1", &planned, Utc::now()).await.expect("upsert");
    for id in ["destroyed", "destroy-failed", "serving"] {
        insert_node(&pool, id, Ownership::Rented, &format!("prov-{id}")).await;
    }
    let provider = |fail: Option<ProviderError>| ObservingProvider {
        pool: pool.clone(),
        row_present_at_destroy: Arc::new(Mutex::new(None)),
        fail,
    };
    store.teardown(&provider(None), &rented("destroyed")).await.expect("teardown");
    store
        .teardown(&provider(Some(ProviderError::Transient("503".into()))), &rented("destroy-failed"))
        .await
        .expect_err("the provider failed");

    // The stale plan: all three re-stated, plus one new node.
    let mut stale = planned.clone();
    stale.push(fanout("fresh", Ownership::Rented, Some(3600)));
    store.upsert_for_broadcast("b1", &stale, Utc::now()).await.expect("upsert");

    assert_eq!(
        desired_ids(&store).await,
        vec!["fresh", "serving"],
        "a desired row came back for a node teardown had already acted on"
    );
}

/// Upsert and teardown take `DESIRED_WRITE_LOCK` first, so they never interleave.
/// Without it, an upsert that read a node's state just before a teardown committed
/// would insert the very row that teardown had just deleted — the same
/// resurrection, through a race in the database instead of a stale snapshot.
#[tokio::test]
async fn an_upsert_waits_for_a_teardown_in_flight_and_then_respects_it() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping an_upsert_waits_for_a_teardown_in_flight_and_then_respects_it");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "n1", Ownership::Rented, "prov-n1").await;

    // A teardown's first step, held open: lock taken, row deleted, node marked —
    // not yet committed.
    let mut in_flight = pool.begin().await.expect("begin");
    for sql in [
        "SELECT pg_advisory_xact_lock($1)",
        "DELETE FROM mm_fleet_desired WHERE mm_node_id = 'n1'",
        "UPDATE mm_fleet_nodes SET state = 'destroying' WHERE mm_node_id = 'n1'",
    ] {
        let q = sqlx::query(sql);
        let q = if sql.contains("$1") { q.bind(DESIRED_WRITE_LOCK) } else { q };
        q.execute(&mut *in_flight).await.expect("teardown step");
    }

    // Meanwhile, a plan built before that teardown re-states n1.
    let upsert = tokio::spawn({
        let store = DesiredStore::new(pool.clone());
        async move {
            store
                .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!upsert.is_finished(), "the upsert ran beside a teardown in flight");

    in_flight.commit().await.expect("commit the teardown");
    upsert.await.expect("join").expect("upsert");
    assert!(
        desired_ids(&store).await.is_empty(),
        "the upsert put back the row the teardown had just deleted"
    );
}

/// The other half of the same lock: a teardown waits for an upsert in flight. (If
/// only one side took it, the other could still slip between a read and a write.)
#[tokio::test]
async fn a_teardown_waits_for_an_upsert_in_flight() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_teardown_waits_for_an_upsert_in_flight");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    wipe(&pool).await;

    let store = DesiredStore::new(pool.clone());
    store
        .upsert_for_broadcast("b1", &[fanout("n1", Ownership::Rented, Some(3600))], Utc::now())
        .await
        .expect("upsert");
    insert_node(&pool, "n1", Ownership::Rented, "prov-n1").await;

    let mut in_flight = pool.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *in_flight)
        .await
        .expect("lock");

    let teardown = tokio::spawn({
        let store = DesiredStore::new(pool.clone());
        let provider = ObservingProvider {
            pool: pool.clone(),
            row_present_at_destroy: Arc::new(Mutex::new(None)),
            fail: None,
        };
        async move { store.teardown(&provider, &rented("n1")).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!teardown.is_finished(), "the teardown ran beside an upsert in flight");

    in_flight.commit().await.expect("release");
    teardown.await.expect("join").expect("teardown");
    assert_eq!(node_state(&pool, "n1").await, NodeState::Gone.as_str());
}
