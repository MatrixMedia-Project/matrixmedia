mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::{NodeFlavor, NodeId, Ownership};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::desired::{DesiredStore, StoreError, TeardownTarget};
use mm_fleet::nodes_db::{self, InsertRefused, NewNode};
use mm_fleet::provider::{DryRunProvider, InstanceHandle, Intent};
use mm_fleet::providers_db::{self as pdb, NewZone, ProviderInput};
use mm_fleet::roles::Purpose;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never
/// land inside a test that is running. Hold the returned guard for the whole test. The pool is
/// widened: the lock tests park a holder and a waiter and poll `pg_stat_activity` besides.
async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = common::wide_pool(try_pool().await?).await;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in [
        "mm_fleet_boot_tokens",
        "mm_fleet_zone_cooldown",
        "mm_fleet_desired",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_requests",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_nodes",
        "mm_fleet_providers",
        "mm_fleet_ops_audit",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    Some((pool, guard))
}

async fn provider(pool: &sqlx::PgPool, cap: i32) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "GPU-S".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: "first".into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: Some("proj-1".into()),
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: None,
            max_gpu_nodes: cap,
            zones: vec![NewZone {
                zone: "z-a".into(),
                region: "eu".into(),
                sizes,
            }],
        },
    )
    .await
    .unwrap()
}

/// A rented desired row, the thing a node insert must find standing.
async fn desire_until(pool: &sqlx::PgPool, id: &str, deadline: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, destroy_deadline, purpose)
         VALUES ($1, 'transcode', 'rented', 'eu', 'GPU-S', $2, 'test_boot')",
    )
    .bind(id)
    .bind(deadline)
    .execute(pool)
    .await
    .unwrap();
}

async fn desire(pool: &sqlx::PgPool, id: &str) {
    desire_until(pool, id, Utc::now() + Duration::minutes(15)).await;
}

fn node<'a>(id: &'a str, provider_ref: &'a str) -> NewNode<'a> {
    NewNode {
        mm_node_id: id,
        provider_ref,
        kind: "scaleway",
        zone: "z-a",
        size: "GPU-S",
        purpose: Purpose::TestBoot,
        destroy_deadline: Utc::now() + Duration::minutes(15),
        created_by: Some("@argi:example"),
    }
}

fn assert_close(a: DateTime<Utc>, b: DateTime<Utc>, what: &str) {
    assert!((a - b).num_seconds().abs() <= 1, "{what}: {a} vs {b}");
}

#[tokio::test]
async fn the_node_row_carries_its_deadline_before_any_create() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let n = node("tb-1", &p);
    desire_until(&pool, "tb-1", n.destroy_deadline).await;
    nodes_db::insert_for_create(&pool, &n, 5).await.unwrap();
    let row = nodes_db::api_node(&pool, "tb-1")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.state, "requested");
    assert_eq!(row.provider_id, None);
    assert_eq!(
        (
            row.provider_ref.as_deref(),
            row.provider_zone.as_deref(),
            row.size.as_deref()
        ),
        (Some(p.as_str()), Some("z-a"), Some("GPU-S"))
    );
    assert_eq!(row.purpose, "test_boot");
    assert_close(
        row.destroy_deadline.unwrap(),
        n.destroy_deadline,
        "copied once, as given",
    );
    assert_eq!(
        nodes_db::may_exist(&pool).await.unwrap().len(),
        1,
        "no handle yet: may exist"
    );
}

#[tokio::test]
async fn caps_are_counted_under_the_lock() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    for id in ["tb-1", "tb-2", "tb-3"] {
        desire(&pool, id).await;
    }
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    assert!(matches!(
        nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5).await,
        Err(InsertRefused::ProviderCap { live: 1, cap: 1 })
    ));
    let q = provider(&pool, 5).await;
    assert!(matches!(
        nodes_db::insert_for_create(&pool, &node("tb-3", &q), 1).await,
        Err(InsertRefused::GlobalCap { live: 1, cap: 1 })
    ));
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'tb-1'")
        .execute(&pool)
        .await
        .unwrap();
    nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5)
        .await
        .expect("a gone node frees its slot");
    assert!(
        matches!(
            nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5).await,
            Err(InsertRefused::AlreadyExists)
        ),
        "a duplicate id is reported as one even though the provider's cap is also reached"
    );
}

/// The per-provider cap is counted AFTER the provider-row lock is won. A count taken before the
/// lock would not see a node insert still in flight and would let the cap be exceeded.
#[tokio::test]
async fn a_cap_is_counted_after_the_lock_is_won_not_before() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-2").await;
    // Another insert paused after its lock and its insert, holding the last slot uncommitted.
    let mut ins = pool.begin().await.unwrap();
    sqlx::query("SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE")
        .bind(&p).fetch_one(&mut *ins).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend, provider_ref)
                 VALUES ('tb-1', 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'api', $1)")
        .bind(&p).execute(&mut *ins).await.unwrap();
    let (pool2, p2) = (pool.clone(), p.clone());
    let second =
        tokio::spawn(
            async move { nodes_db::insert_for_create(&pool2, &node("tb-2", &p2), 5).await },
        );
    common::wait_until_blocked(&pool, "mm_fleet_providers", 1).await;
    ins.commit().await.unwrap();
    assert!(
        matches!(
            second.await.unwrap(),
            Err(InsertRefused::ProviderCap { live: 1, cap: 1 })
        ),
        "the waiter counts the node it waited for"
    );
    assert!(nodes_db::api_node(&pool, "tb-2").await.unwrap().is_none());
}

#[tokio::test]
async fn a_node_insert_waits_for_a_delete_in_flight_and_then_refuses() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-1").await;
    let mut del = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR UPDATE")
        .bind(&p)
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET deleted_at = now(), enabled = false WHERE id = $1")
        .bind(&p)
        .execute(&mut *del)
        .await
        .unwrap();
    let (pool2, p2) = (pool.clone(), p.clone());
    let insert =
        tokio::spawn(
            async move { nodes_db::insert_for_create(&pool2, &node("tb-1", &p2), 5).await },
        );
    common::wait_until_blocked(&pool, "mm_fleet_providers", 1).await;
    del.commit().await.unwrap();
    assert!(matches!(
        insert.await.unwrap(),
        Err(InsertRefused::ProviderGone)
    ));
    assert!(nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none());
}

#[tokio::test]
async fn a_delete_waits_for_a_node_insert_in_flight_and_then_refuses() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    // insert_for_create paused after its lock and its insert.
    let mut ins = pool.begin().await.unwrap();
    sqlx::query("SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE")
        .bind(&p).fetch_one(&mut *ins).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend, provider_ref)
                 VALUES ('tb-1', 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'api', $1)")
        .bind(&p).execute(&mut *ins).await.unwrap();
    let (pool2, p2) = (pool.clone(), p.clone());
    let del = tokio::spawn(async move { pdb::soft_delete(&pool2, &p2).await });
    common::wait_until_blocked(&pool, "mm_fleet_providers", 1).await;
    ins.commit().await.unwrap();
    assert!(
        matches!(del.await.unwrap(), Err(pdb::DeleteRefused::NodesExist(1))),
        "the delete counts the node it waited for"
    );
}

#[tokio::test]
async fn no_node_is_inserted_without_a_standing_rented_desired_row() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    let refused = nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap_err();
    assert!(matches!(refused, InsertRefused::NotDesired));
    assert_eq!(
        refused.to_string(),
        "no desired row: its teardown was ordered"
    );
    assert!(nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none());
    // A desired row that is not rented is not one a paid machine is created for.
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, purpose)
         VALUES ('tb-2', 'transcode', 'owned', 'eu', 'GPU-S', 'test_boot')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5).await,
        Err(InsertRefused::NotDesired)
    ));
    assert!(nodes_db::api_node(&pool, "tb-2").await.unwrap().is_none());
}

#[tokio::test]
async fn a_node_insert_waits_for_a_teardown_in_flight_and_then_refuses() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    // order_teardown's first statement, not yet committed.
    let mut td = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM mm_fleet_desired WHERE mm_node_id = 'tb-1'")
        .execute(&mut *td)
        .await
        .unwrap();
    let (pool2, p2) = (pool.clone(), p.clone());
    let insert =
        tokio::spawn(
            async move { nodes_db::insert_for_create(&pool2, &node("tb-1", &p2), 5).await },
        );
    common::wait_until_blocked(&pool, "mm_fleet_desired", 1).await;
    td.commit().await.unwrap();
    assert!(
        matches!(insert.await.unwrap(), Err(InsertRefused::NotDesired)),
        "the insert saw the delete it waited for"
    );
    assert!(nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none());
}

#[tokio::test]
async fn a_teardown_after_the_insert_finds_the_node_and_orders_it_destroyed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    let before = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(before.state, "requested");
    // The real `order_teardown`, after the insert committed. It deletes the desired row, then
    // marks the node in a statement of its own, which sees the row the insert wrote.
    DesiredStore::new(pool.clone())
        .order_teardown(&TeardownTarget {
            mm_node_id: NodeId::new("tb-1"),
            ownership: Ownership::Rented,
            flavor: NodeFlavor::Transcode,
            provider_id: None,
        })
        .await
        .unwrap();
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        row.state, "destroying",
        "the teardown finds the node the insert wrote"
    );
    let desired_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE mm_node_id = 'tb-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(desired_left, 0, "and its desired row is gone");
}

/// The mirror of the test above: the teardown arrives while the insert is still in flight. The
/// real `order_teardown` waits for it on the DELETE, and its node UPDATE, a statement of its own
/// that starts after that wait, sees the node row the insert committed. A single statement
/// would take its snapshot first, miss the row, and leave the node `requested` with no desired
/// row and no teardown.
#[tokio::test]
async fn a_teardown_ordered_during_an_insert_in_flight_marks_the_node_it_waited_for() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    // insert_for_create paused after its desired-row lock and its INSERT.
    let mut ins = pool.begin().await.unwrap();
    sqlx::query("SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = 'tb-1' AND ownership = 'rented' FOR KEY SHARE")
        .fetch_one(&mut *ins)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend, provider_ref)
                 VALUES ('tb-1', 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'api', $1)")
        .bind(&p)
        .execute(&mut *ins)
        .await
        .unwrap();
    let store = DesiredStore::new(pool.clone());
    let order = tokio::spawn(async move {
        store
            .order_teardown(&TeardownTarget {
                mm_node_id: NodeId::new("tb-1"),
                ownership: Ownership::Rented,
                flavor: NodeFlavor::Transcode,
                provider_id: None,
            })
            .await
    });
    common::wait_until_blocked(&pool, "mm_fleet_desired", 1).await;
    ins.commit().await.unwrap();
    order.await.unwrap().unwrap();
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        row.state, "destroying",
        "the node the teardown waited for is marked, not left requested"
    );
}

/// The completing side of the same family of races. `complete_teardown` reads a node with no
/// handle, so it has nothing to destroy; the create then returns and records its handle (what
/// `mark_created` writes, state kept `destroying`); and only then does the completion mark the
/// node. Marking `gone` regardless would leave a live machine behind a `gone` row, which the
/// deadline sweeper skips and the orphan sweeper counts as known. The mark is therefore
/// conditional on the handle the completion read, and a lost race is an error, so the node stays
/// `destroying` and the next pass destroys the new handle.
#[tokio::test]
async fn a_handle_recorded_while_a_teardown_completes_is_destroyed_by_the_next_pass() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    let target = TeardownTarget {
        mm_node_id: NodeId::new("tb-1"),
        ownership: Ownership::Rented,
        flavor: NodeFlavor::Transcode,
        provider_id: None,
    };
    DesiredStore::new(pool.clone())
        .order_teardown(&target)
        .await
        .unwrap();
    // The create is about to record its handle: hold the node row so the completion's mark waits.
    let mut create = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM mm_fleet_nodes WHERE mm_node_id = 'tb-1' FOR UPDATE")
        .execute(&mut *create)
        .await
        .unwrap();
    let provider_calls = Arc::new(DryRunProvider::new());
    let first = tokio::spawn({
        let (store, provider_calls, target) = (
            DesiredStore::new(pool.clone()),
            provider_calls.clone(),
            target.clone(),
        );
        async move { store.complete_teardown(&*provider_calls, &target).await }
    });
    // Its read (a plain SELECT) has returned no handle by the time its mark is blocked.
    common::wait_until_blocked(&pool, "mm_fleet_nodes", 1).await;
    sqlx::query("UPDATE mm_fleet_nodes SET provider_id = 'z-a/late', billing_started_at = now() WHERE mm_node_id = 'tb-1'")
        .execute(&mut *create)
        .await
        .unwrap();
    create.commit().await.unwrap();
    let err = first.await.unwrap().unwrap_err();
    assert!(
        matches!(err, StoreError::HandleRecordedMeanwhile { .. }),
        "{err}"
    );
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        row.state, "destroying",
        "never gone with a handle nobody destroyed"
    );
    assert_eq!(row.provider_id.as_deref(), Some("z-a/late"));
    assert!(provider_calls.intents().is_empty());

    // The next pass reads the handle, destroys it, and only then closes the node.
    DesiredStore::new(pool.clone())
        .complete_teardown(&*provider_calls, &target)
        .await
        .unwrap();
    assert_eq!(
        provider_calls.intents(),
        vec![Intent::Destroy("z-a/late".into())]
    );
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(row.state, "gone");
}

#[tokio::test]
async fn the_deadline_is_the_earlier_of_the_desired_rows_and_the_callers() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 3).await;
    let soon = Utc::now() + Duration::minutes(5);
    let later = Utc::now() + Duration::minutes(30);
    desire_until(&pool, "tb-1", soon).await;
    desire_until(&pool, "tb-2", later).await;
    let desired_earlier = NewNode {
        destroy_deadline: later,
        ..node("tb-1", &p)
    };
    nodes_db::insert_for_create(&pool, &desired_earlier, 5)
        .await
        .unwrap();
    let caller_earlier = NewNode {
        destroy_deadline: soon,
        ..node("tb-2", &p)
    };
    nodes_db::insert_for_create(&pool, &caller_earlier, 5)
        .await
        .unwrap();
    let one = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_close(
        one.destroy_deadline.unwrap(),
        soon,
        "the desired row's earlier deadline wins",
    );
    let two = nodes_db::api_node(&pool, "tb-2").await.unwrap().unwrap();
    assert_close(
        two.destroy_deadline.unwrap(),
        soon,
        "the caller's earlier deadline wins",
    );
}

#[tokio::test]
async fn a_duplicate_id_written_meanwhile_under_another_providers_lock_is_already_exists() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    let q = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    // An insert for the same id under provider q, paused before its commit.
    let mut other = pool.begin().await.unwrap();
    sqlx::query("SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&q)
        .fetch_one(&mut *other)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend, provider_ref)
                 VALUES ('tb-1', 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'api', $1)")
        .bind(&q).execute(&mut *other).await.unwrap();
    let (pool2, p2) = (pool.clone(), p.clone());
    let dup =
        tokio::spawn(
            async move { nodes_db::insert_for_create(&pool2, &node("tb-1", &p2), 5).await },
        );
    common::wait_until_blocked(&pool, "INSERT INTO mm_fleet_nodes", 1).await;
    other.commit().await.unwrap();
    assert!(
        matches!(dup.await.unwrap(), Err(InsertRefused::AlreadyExists)),
        "the duplicate is a refusal, not a database error"
    );
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(row.provider_ref.as_deref(), Some(q.as_str()));
}

#[tokio::test]
async fn the_provider_kind_comes_from_the_locked_provider_row() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-1").await;
    let wrong = NewNode {
        kind: "runpod",
        ..node("tb-1", &p)
    };
    nodes_db::insert_for_create(&pool, &wrong, 5).await.unwrap();
    let kind: String =
        sqlx::query_scalar("SELECT provider FROM mm_fleet_nodes WHERE mm_node_id = 'tb-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kind, "scaleway",
        "the provider row's kind, not the caller's"
    );
}

#[tokio::test]
async fn a_handle_is_recorded_even_when_teardown_was_ordered_meanwhile() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    // An operator released it while the create was in flight.
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'destroying' WHERE mm_node_id = 'tb-1'")
        .execute(&pool)
        .await
        .unwrap();
    let h = InstanceHandle {
        provider_id: "z-a/uuid-1".into(),
        public_ip: Some("not an ip".into()),
        created_at: Some(Utc::now()),
    };
    assert!(nodes_db::mark_created(&pool, "tb-1", &h).await.unwrap());
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        row.provider_id.as_deref(),
        Some("z-a/uuid-1"),
        "the destroy that follows needs the handle"
    );
    assert_eq!(
        row.state, "destroying",
        "a release is not undone by a create returning"
    );
    assert!(row.billing_started_at.is_some());
}

#[tokio::test]
async fn a_handle_written_onto_a_gone_row_gets_it_destroyed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    // The row was closed out while the create was in flight.
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'tb-1'")
        .execute(&pool)
        .await
        .unwrap();
    let h = InstanceHandle {
        provider_id: "z-a/uuid-1".into(),
        public_ip: None,
        created_at: None,
    };
    assert!(nodes_db::mark_created(&pool, "tb-1", &h).await.unwrap());
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(row.provider_id.as_deref(), Some("z-a/uuid-1"));
    assert_eq!(
        row.state, "destroying",
        "a machine that exists is not left invisible as gone"
    );
    assert_eq!(
        nodes_db::api_nodes_live(&pool).await.unwrap().len(),
        1,
        "the next teardown pass sees it"
    );
}

#[tokio::test]
async fn the_first_handle_wins() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    let first = InstanceHandle {
        provider_id: "z-a/first".into(),
        public_ip: Some("10.0.0.1".into()),
        created_at: None,
    };
    assert!(nodes_db::mark_created(&pool, "tb-1", &first).await.unwrap());
    let before = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    let second = InstanceHandle {
        provider_id: "z-a/second".into(),
        public_ip: Some("10.0.0.2".into()),
        created_at: None,
    };
    assert!(
        !nodes_db::mark_created(&pool, "tb-1", &second)
            .await
            .unwrap(),
        "a second handle writes nothing"
    );
    let after = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        after, before,
        "the first handle, its state and its billing start stand"
    );
    assert_eq!(after.provider_id.as_deref(), Some("z-a/first"));
    let ip: String =
        sqlx::query_scalar("SELECT host(public_ip) FROM mm_fleet_nodes WHERE mm_node_id = 'tb-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ip, "10.0.0.1");
}

#[tokio::test]
async fn only_a_node_with_no_handle_can_be_forgotten() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    desire(&pool, "tb-2").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5)
        .await
        .unwrap();
    let h = InstanceHandle {
        provider_id: "z-a/uuid-2".into(),
        public_ip: None,
        created_at: None,
    };
    nodes_db::mark_created(&pool, "tb-2", &h).await.unwrap();
    assert!(nodes_db::forget_uncreated(&pool, "tb-1").await.unwrap());
    assert!(
        !nodes_db::forget_uncreated(&pool, "tb-2").await.unwrap(),
        "a machine exists: never forget it"
    );
}

/// Five node rows, one per way a row can fail to be "a create whose outcome is unknown":
/// `tb-a` is the one that is; `tb-b` has a handle and is booting; `tb-c` had its teardown
/// ordered while the create was in flight (no handle, `destroying`); `tb-d` has a handle but is
/// still `requested`; `tb-e` was made by Terraform, not through a provider API.
async fn the_five_rows(pool: &sqlx::PgPool, p: &str) {
    for id in ["tb-a", "tb-b", "tb-c", "tb-d"] {
        desire(pool, id).await;
        nodes_db::insert_for_create(pool, &node(id, p), 10)
            .await
            .unwrap();
    }
    let h = InstanceHandle {
        provider_id: "z-a/b".into(),
        public_ip: None,
        created_at: None,
    };
    nodes_db::mark_created(pool, "tb-b", &h).await.unwrap();
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'destroying' WHERE mm_node_id = 'tb-c'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_nodes SET provider_id = 'z-a/d' WHERE mm_node_id = 'tb-d'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend)
         VALUES ('tb-e', 'origin', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'terraform')",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn may_exist_is_only_a_create_whose_outcome_is_unknown() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 5).await;
    the_five_rows(&pool, &p).await;
    let ids: Vec<String> = nodes_db::may_exist(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.mm_node_id)
        .collect();
    assert_eq!(
        ids,
        vec!["tb-a".to_string()],
        "not a booting row with a handle, not an ordered teardown, not a requested row that has a handle, not Terraform's"
    );
}

#[tokio::test]
async fn forget_uncreated_never_touches_a_row_with_a_handle_an_ordered_teardown_or_terraform() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 5).await;
    the_five_rows(&pool, &p).await;
    for id in ["tb-b", "tb-c", "tb-d", "tb-e"] {
        assert!(
            !nodes_db::forget_uncreated(&pool, id).await.unwrap(),
            "{id} must not be forgotten"
        );
    }
    let left: Vec<String> =
        sqlx::query_scalar("SELECT mm_node_id FROM mm_fleet_nodes ORDER BY mm_node_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left, ["tb-a", "tb-b", "tb-c", "tb-d", "tb-e"]);
    assert!(nodes_db::forget_uncreated(&pool, "tb-a").await.unwrap());
    let left: Vec<String> =
        sqlx::query_scalar("SELECT mm_node_id FROM mm_fleet_nodes ORDER BY mm_node_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left, ["tb-b", "tb-c", "tb-d", "tb-e"]);
}

#[tokio::test]
async fn pending_rows_are_desired_rented_rows_without_a_node() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    for id in ["tb-1", "tb-2"] {
        sqlx::query("INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, destroy_deadline, purpose, pinned_provider_id, pinned_zone)
                     VALUES ($1, 'transcode', 'rented', 'eu', 'GPU-S', now() + interval '15 minutes', 'test_boot', $2, 'z-a')")
            .bind(id).bind(&p).execute(&pool).await.unwrap();
    }
    nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5)
        .await
        .unwrap();
    let pending = nodes_db::pending_desired(&pool).await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|d| d.mm_node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["tb-1"]
    );
    assert_eq!(pending[0].pinned_zone.as_deref(), Some("z-a"));
}

#[tokio::test]
async fn the_gpu_nodes_view_lists_live_rented_gpu_nodes_with_their_provider() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    desire(&pool, "tb-2").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'tb-2'")
        .execute(&pool)
        .await
        .unwrap();
    let rows = nodes_db::gpu_nodes_view(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider_label.as_deref(), Some("first"));
    assert_eq!(pdb::live_nodes_for(&pool, &p).await.unwrap(), 1);
    assert_eq!(
        pdb::live_node_zones(&pool, &p).await.unwrap(),
        vec!["z-a".to_string()]
    );
}

#[tokio::test]
async fn the_live_node_reads_run_inside_a_transaction_that_holds_the_provider_lock() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
    desire(&pool, "tb-1").await;
    nodes_db::insert_for_create(&pool, &node("tb-1", &p), 5)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&p)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(pdb::live_nodes_for(&mut *tx, &p).await.unwrap(), 1);
    assert_eq!(
        pdb::live_node_zones(&mut *tx, &p).await.unwrap(),
        vec!["z-a".to_string()]
    );
    tx.rollback().await.unwrap();
}
