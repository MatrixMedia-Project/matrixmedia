mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::desired::DESIRED_WRITE_LOCK;
use mm_fleet::providers_db::{self as pdb, NewZone, ProviderInput};
use mm_fleet::requests_db as rq;
use mm_fleet::test_boot::{self, token_hash};
use mm_fleet::test_boot_db::{self, NewTestBoot, TestBootRefused};
use serde_json::json;
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

async fn provider_named(pool: &sqlx::PgPool, label: &str, cap: i32) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "GPU-S".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: label.into(),
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

async fn provider(pool: &sqlx::PgPool, cap: i32) -> String {
    provider_named(pool, "first", cap).await
}

fn boot<'a>(provider_id: &'a str, per_day: i64, global_cap: i64) -> NewTestBoot<'a> {
    NewTestBoot {
        provider_id,
        zone: "z-a",
        region: "eu",
        size: "GPU-S",
        reason: "prove the zone",
        requested_by: "@argi:example",
        report_url: "https://mm.example/_mm/webhooks/fleet/boot-report",
        per_day,
        global_cap,
    }
}

async fn finish_request(pool: &sqlx::PgPool, id: &str) {
    sqlx::query("UPDATE mm_fleet_requests SET state = 'done' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_test_boot_is_a_request_and_a_pinned_desired_row_with_a_hard_deadline() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let (rid, node) = test_boot_db::create(&pool, &boot(&p, 5, 1)).await.unwrap();
    assert_eq!(node, test_boot::node_id_for(&rid));
    let r = rq::get(&pool, &rid).await.unwrap().unwrap();
    assert_eq!(
        (r.kind.as_str(), r.state.as_str(), r.zone.as_deref()),
        ("test_boot", "queued", Some("z-a"))
    );
    assert_eq!(
        (r.role.as_deref(), r.requested_by.as_str()),
        (Some("transcode"), "@argi:example")
    );
    assert_eq!(
        r.params["report_url"],
        "https://mm.example/_mm/webhooks/fleet/boot-report"
    );
    let (purpose, pinned, zone, left_secs): (String, String, String, f64) = sqlx::query_as(
        "SELECT purpose, pinned_provider_id, pinned_zone,
                    EXTRACT(EPOCH FROM (destroy_deadline - now()))::float8
               FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(node.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (purpose.as_str(), pinned.as_str(), zone.as_str()),
        ("test_boot", p.as_str(), "z-a")
    );
    // Both sides on the database's clock: the host's can differ from it by minutes.
    assert!(
        left_secs > 14.0 * 60.0 && left_secs <= 15.0 * 60.0,
        "15-minute hard deadline, got {left_secs} s left"
    );
}

#[tokio::test]
async fn one_test_boot_at_a_time_and_a_daily_limit() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 5).await;
    let (first, _) = test_boot_db::create(&pool, &boot(&p, 2, 5)).await.unwrap();
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 2, 5)).await,
        Err(TestBootRefused::AlreadyRunning)
    ));
    finish_request(&pool, &first).await;
    let (second, _) = test_boot_db::create(&pool, &boot(&p, 2, 5)).await.unwrap();
    finish_request(&pool, &second).await;
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 2, 5)).await,
        Err(TestBootRefused::DailyLimit { used: 2, limit: 2 })
    ));
    // A refusal writes nothing.
    assert_eq!(count(&pool, "mm_fleet_requests").await, 2);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 2);
}

#[tokio::test]
async fn a_running_boot_blocks_the_next_for_its_whole_claim_and_a_dead_runners_does_not() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 5).await;
    let (first, _) = test_boot_db::create(&pool, &boot(&p, 5, 5)).await.unwrap();
    rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("claimed");
    // Ten minutes into a boot is normal: the machine is still being paid for.
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '10 minutes' WHERE id = $1",
    )
    .bind(&first)
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 5, 5)).await,
        Err(TestBootRefused::AlreadyRunning)
    ));
    // Past its claim the runner is dead and the request is not "running" any more.
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '26 minutes' WHERE id = $1",
    )
    .bind(&first)
    .execute(&pool)
    .await
    .unwrap();
    test_boot_db::create(&pool, &boot(&p, 5, 5))
        .await
        .expect("a dead runner's claim blocks nothing");
}

#[tokio::test]
async fn both_gpu_caps_count_running_machines_and_the_global_one_counts_unstarted_boots() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend)
                 VALUES ('bc-x-transcode-0', 'transcode', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $1, 'api')")
        .bind(&p).execute(&pool).await.unwrap();
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 5, 1)).await,
        Err(TestBootRefused::GlobalCap { live: 1, cap: 1 })
    ));
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 5, 5)).await,
        Err(TestBootRefused::ProviderCap { live: 1, cap: 1 })
    ));
    // A machine that is gone holds no slot.
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'bc-x-transcode-0'")
        .execute(&pool)
        .await
        .unwrap();
    test_boot_db::create(&pool, &boot(&p, 5, 1))
        .await
        .expect("a gone machine is not live");
}

#[tokio::test]
async fn an_unstarted_boot_holds_a_global_slot_until_its_machine_exists_and_then_counts_once() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 5).await;
    let (first, node) = test_boot_db::create(&pool, &boot(&p, 5, 5)).await.unwrap();
    finish_request(&pool, &first).await;
    // Its desired row stands, its machine does not exist yet: already spoken for.
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 5, 1)).await,
        Err(TestBootRefused::GlobalCap { live: 1, cap: 1 })
    ));
    // Once the node row exists it is counted as a node, not as the desired row too.
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend, purpose)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '15 minutes', $2, 'api', 'test_boot')")
        .bind(node.as_str()).bind(&p).execute(&pool).await.unwrap();
    test_boot_db::create(&pool, &boot(&p, 5, 2))
        .await
        .expect("one slot is held, not two");
}

#[tokio::test]
async fn a_deleted_provider_takes_no_test_boot() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let gone = provider(&pool, 1).await;
    pdb::soft_delete(&pool, &gone).await.unwrap();
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&gone, 5, 5)).await,
        Err(TestBootRefused::ProviderGone)
    ));
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);
}

/// `create` takes the desired-set lock and then the provider row. A writer holding the
/// desired-set lock (`set_order`, `order_teardown`) must be able to take that provider row
/// while a `create` waits for the lock: if `create` held the row first, the two would wait
/// on each other.
#[tokio::test]
async fn create_takes_the_desired_set_lock_before_the_provider_row() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let create = tokio::spawn({
        let (pool, p) = (pool.clone(), p.clone());
        async move { test_boot_db::create(&pool, &boot(&p, 5, 5)).await }
    });
    common::wait_until_blocked(&pool, "pg_advisory_xact_lock", 1).await;
    // A bounded wait, so a lock cycle fails this test instead of hanging it.
    sqlx::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET updated_at = now() WHERE id = $1")
        .bind(&p)
        .execute(&mut *holder)
        .await
        .expect("the provider row is free while create waits for the desired-set lock");
    holder.commit().await.unwrap();
    create
        .await
        .unwrap()
        .expect("create ran once the lock was free");
}

/// The desired-set lock makes the one-at-a-time limit exact even for two different providers,
/// whose rows do not conflict with each other.
#[tokio::test]
async fn two_test_boots_queued_at_once_run_one_after_the_other() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider_named(&pool, "first", 5).await;
    let q = provider_named(&pool, "second", 5).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let spawn_create = |provider_id: String| {
        let pool = pool.clone();
        tokio::spawn(async move { test_boot_db::create(&pool, &boot(&provider_id, 5, 5)).await })
    };
    let (a, b) = (spawn_create(p), spawn_create(q));
    common::wait_until_blocked(&pool, "pg_advisory_xact_lock", 2).await;
    holder.commit().await.unwrap();
    let results = [a.await.unwrap(), b.await.unwrap()];
    let queued = results.iter().filter(|r| r.is_ok()).count();
    let refused = results
        .iter()
        .filter(|r| matches!(r, Err(TestBootRefused::AlreadyRunning)))
        .count();
    assert_eq!((queued, refused), (1, 1), "{results:?}");
    assert_eq!(count(&pool, "mm_fleet_requests").await, 1);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 1);
}

/// A provider delete in flight holds the row `FOR UPDATE`; the test boot waits for it and then
/// finds no live provider. Without the provider-row lock the boot would read the row as it was
/// and queue a machine for a provider that is going away.
#[tokio::test]
async fn a_test_boot_waits_for_a_provider_delete_in_flight_and_then_refuses() {
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
    let create = tokio::spawn({
        let (pool, p) = (pool.clone(), p.clone());
        async move { test_boot_db::create(&pool, &boot(&p, 5, 5)).await }
    });
    common::wait_until_blocked(&pool, "FOR NO KEY UPDATE", 1).await;
    del.commit().await.unwrap();
    assert!(matches!(
        create.await.unwrap(),
        Err(TestBootRefused::ProviderGone)
    ));
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);
}

/// A node insert in flight (the runner's `insert_for_create`) holds the provider row; the
/// provider cap is counted after the test boot has waited for it, so the node is counted.
#[tokio::test]
async fn the_provider_cap_is_counted_after_a_node_insert_in_flight_commits() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let mut ins = pool.begin().await.unwrap();
    sqlx::query("SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE")
        .bind(&p).fetch_one(&mut *ins).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, created_backend, provider_ref)
                 VALUES ('bc-x-transcode-0', 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', 'api', $1)")
        .bind(&p).execute(&mut *ins).await.unwrap();
    let create = tokio::spawn({
        let (pool, p) = (pool.clone(), p.clone());
        async move { test_boot_db::create(&pool, &boot(&p, 5, 5)).await }
    });
    common::wait_until_blocked(&pool, "FOR NO KEY UPDATE", 1).await;
    ins.commit().await.unwrap();
    assert!(
        matches!(
            create.await.unwrap(),
            Err(TestBootRefused::ProviderCap { live: 1, cap: 1 })
        ),
        "the waiter counts the node it waited for"
    );
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
}

async fn test_boot_node(pool: &sqlx::PgPool, id: &str, purpose: &str) {
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, purpose, created_backend)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '15 minutes', $2, 'api')")
        .bind(id).bind(purpose).execute(pool).await.unwrap();
}

#[tokio::test]
async fn a_report_token_works_once_and_never_after_its_deadline() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let node = mm_core::fleet::NodeId::new("tb-abc");
    test_boot_node(&pool, "tb-abc", "test_boot").await;
    let (token, hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &hash, Utc::now() + Duration::minutes(15))
        .await
        .unwrap();
    let stored = json!({"report": {"v": 1}, "received_at": "2026-10-07T12:00:00Z"});
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&token), &stored)
            .await
            .unwrap()
            .as_deref(),
        Some("tb-abc")
    );
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&token), &stored)
            .await
            .unwrap(),
        None,
        "single use"
    );
    let report: serde_json::Value =
        sqlx::query_scalar("SELECT boot_report FROM mm_fleet_nodes WHERE mm_node_id = 'tb-abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(report, stored);

    let (late, late_hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &late_hash, Utc::now() + Duration::minutes(15))
        .await
        .unwrap();
    // Expired by the database's clock, whatever the host's says.
    sqlx::query("UPDATE mm_fleet_boot_tokens SET expires_at = now() - interval '1 second' WHERE mm_node_id = 'tb-abc'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&late), &stored)
            .await
            .unwrap(),
        None,
        "expired"
    );
    assert_eq!(
        test_boot_db::accept_report(&pool, &[0u8; 32], &stored)
            .await
            .unwrap(),
        None,
        "unknown"
    );
}

#[tokio::test]
async fn a_new_token_replaces_the_old_one_and_a_dropped_token_redeems_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let node = mm_core::fleet::NodeId::new("tb-abc");
    test_boot_node(&pool, "tb-abc", "test_boot").await;
    let until = Utc::now() + Duration::minutes(15);
    let (old, old_hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &old_hash, until)
        .await
        .unwrap();
    let (new, new_hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &new_hash, until)
        .await
        .unwrap();
    let stored = json!({"report": {"v": 1}});
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&old), &stored)
            .await
            .unwrap(),
        None,
        "the replaced token is unknown"
    );
    test_boot_db::drop_token(&pool, &node).await.unwrap();
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&new), &stored)
            .await
            .unwrap(),
        None,
        "a dropped token redeems nothing"
    );
    let report: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT boot_report FROM mm_fleet_nodes WHERE mm_node_id = 'tb-abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(report, None, "nothing was attached");
    assert_eq!(count(&pool, "mm_fleet_boot_tokens").await, 0);
}

/// Only the hash is kept: the table has no column the token itself could sit in, and the
/// stored bytes are the hash and nothing else.
#[tokio::test]
async fn only_the_hash_is_stored() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let node = mm_core::fleet::NodeId::new("tb-abc");
    test_boot_node(&pool, "tb-abc", "test_boot").await;
    let (token, hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &hash, Utc::now() + Duration::minutes(15))
        .await
        .unwrap();
    let kept: Vec<u8> = sqlx::query_scalar(
        "SELECT token_hash FROM mm_fleet_boot_tokens WHERE mm_node_id = 'tb-abc'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, token_hash(&token));
    assert_ne!(kept, token.as_bytes());
    assert_eq!(kept.len(), 32);
}

/// A report that cannot be attached (no test-boot node to attach it to) is not accepted, and
/// its token is not spent: the caller cannot tell it from any other refusal.
#[tokio::test]
async fn a_report_that_cannot_be_attached_does_not_spend_its_token() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    test_boot_node(&pool, "bc-x-transcode-0", "broadcast").await;
    let stored = json!({"report": {"v": 1}});
    for id in ["bc-x-transcode-0", "tb-missing"] {
        let node = mm_core::fleet::NodeId::new(id);
        let (token, hash) = test_boot::mint_token();
        sqlx::query("DELETE FROM mm_fleet_boot_tokens")
            .execute(&pool)
            .await
            .unwrap();
        test_boot_db::store_token(&pool, &node, &hash, Utc::now() + Duration::minutes(15))
            .await
            .unwrap();
        assert_eq!(
            test_boot_db::accept_report(&pool, &token_hash(&token), &stored)
                .await
                .unwrap(),
            None,
            "{id}"
        );
        let used: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT used_at FROM mm_fleet_boot_tokens WHERE mm_node_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(used, None, "{id}: the token was not spent");
    }
}

/// A retry that stores the same hash again must not bring a redeemed token back.
#[tokio::test]
async fn a_spent_token_stays_spent_when_the_same_hash_is_stored_again() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let node = mm_core::fleet::NodeId::new("tb-abc");
    test_boot_node(&pool, "tb-abc", "test_boot").await;
    let until = Utc::now() + Duration::minutes(15);
    let (token, hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &hash, until)
        .await
        .unwrap();
    let stored = json!({"report": {"v": 1}});
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&token), &stored)
            .await
            .unwrap()
            .as_deref(),
        Some("tb-abc")
    );

    test_boot_db::store_token(&pool, &node, &hash, until + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&token), &stored)
            .await
            .unwrap(),
        None,
        "the same hash stored again is still spent"
    );

    // A different hash is a new token, and it works once.
    let (fresh, fresh_hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &fresh_hash, until)
        .await
        .unwrap();
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&fresh), &stored)
            .await
            .unwrap()
            .as_deref(),
        Some("tb-abc")
    );
    assert_eq!(
        test_boot_db::accept_report(&pool, &token_hash(&fresh), &stored)
            .await
            .unwrap(),
        None
    );
}

/// Two redemptions of one token at once: the second waits for the first's row lock, then finds
/// the token spent.
#[tokio::test]
async fn two_redemptions_of_one_token_at_once_redeem_it_once() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let node = mm_core::fleet::NodeId::new("tb-abc");
    test_boot_node(&pool, "tb-abc", "test_boot").await;
    let (token, hash) = test_boot::mint_token();
    test_boot_db::store_token(&pool, &node, &hash, Utc::now() + Duration::minutes(15))
        .await
        .unwrap();
    let stored = json!({"report": {"v": 1}});

    // A redemption paused after it took the token, before it commits.
    let mut first = pool.begin().await.unwrap();
    let redeemed: Option<String> = sqlx::query_scalar(
        "UPDATE mm_fleet_boot_tokens SET used_at = now()
          WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
          RETURNING mm_node_id",
    )
    .bind(token_hash(&token))
    .fetch_optional(&mut *first)
    .await
    .unwrap();
    assert_eq!(redeemed.as_deref(), Some("tb-abc"));

    let second = tokio::spawn({
        let (pool, hash, stored) = (pool.clone(), token_hash(&token), stored.clone());
        async move { test_boot_db::accept_report(&pool, &hash, &stored).await }
    });
    common::wait_until_blocked(&pool, "mm_fleet_boot_tokens", 1).await;
    first.commit().await.unwrap();
    assert_eq!(
        second.await.unwrap().unwrap(),
        None,
        "the waiter finds the token spent"
    );
    let attached: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT boot_report FROM mm_fleet_nodes WHERE mm_node_id = 'tb-abc'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        attached, None,
        "only a redemption that won attaches a report"
    );
}

/// The caps count the same machines as the runner's `insert_for_create`: rented transcode nodes
/// that are not gone. Machines of another flavor, ownership or state hold no slot.
#[tokio::test]
async fn the_caps_count_the_machines_the_runners_insert_counts() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, 1).await;
    let q = provider_named(&pool, "second", 5).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend)
                 VALUES ('bc-live', 'transcode', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $1, 'api'),
                        ('bc-gone', 'transcode', 'rented', 'scaleway', 'gone', now() + interval '1 hour', $1, 'api'),
                        ('bc-edge', 'edge', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $1, 'api')")
        .bind(&p).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, provider_ref, created_backend)
                 VALUES ('bc-owned', 'transcode', 'owned', 'scaleway', 'healthy', $1, 'api')")
        .bind(&p).execute(&pool).await.unwrap();
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&p, 5, 5)).await,
        Err(TestBootRefused::ProviderCap { live: 1, cap: 1 })
    ));
    assert!(matches!(
        test_boot_db::create(&pool, &boot(&q, 5, 1)).await,
        Err(TestBootRefused::GlobalCap { live: 1, cap: 1 })
    ));
}
