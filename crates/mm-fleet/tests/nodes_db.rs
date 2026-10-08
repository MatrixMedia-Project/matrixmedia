mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::nodes_db::{self, InsertRefused, NewNode};
use mm_fleet::provider::InstanceHandle;
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

#[tokio::test]
async fn the_node_row_carries_its_deadline_before_any_create() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let n = node("tb-1", &p);
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
    assert!(
        (row.destroy_deadline.unwrap() - n.destroy_deadline)
            .num_seconds()
            .abs()
            <= 1,
        "copied once, as given"
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
    assert!(matches!(
        nodes_db::insert_for_create(&pool, &node("tb-2", &p), 5).await,
        Err(InsertRefused::AlreadyExists)
    ));
}

#[tokio::test]
async fn a_node_insert_waits_for_a_delete_in_flight_and_then_refuses() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
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

/// The per-provider cap is counted AFTER the provider-row lock is won. A count taken before the
/// lock would not see a node insert still in flight and would let the cap be exceeded.
#[tokio::test]
async fn a_cap_is_counted_after_the_lock_is_won_not_before() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
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
async fn a_handle_is_recorded_even_when_teardown_was_ordered_meanwhile() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
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
async fn only_a_node_with_no_handle_can_be_forgotten() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 2).await;
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
