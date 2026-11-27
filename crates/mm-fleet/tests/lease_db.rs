//! PG-gated coverage for manual relay leases (Task 7).
//!
//! The pure extension decision (`should_extend`) has its own table tests in
//! `mm-fleet/src/lease_db.rs`; what needs a database is the idempotent create (Review Focus 4)
//! and the release state transitions.

use std::sync::OnceLock;

use chrono::{DateTime, Duration, SubsecRound, Utc};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::lease_db::{self, AutoscaleLease, LeaseCreate, LeaseError, ReleaseState};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never land
/// inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in ["mm_fleet_nodes", "mm_fleet_desired"] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    Some((pool, guard))
}

fn req(request_id: Uuid, count: u8) -> LeaseCreate {
    LeaseCreate {
        request_id,
        region: "eu-ams".into(),
        provider_id: None,
        count,
        created_by: "operator@example.com".into(),
        duration_hint: None,
    }
}

async fn row(
    pool: &PgPool,
    id: &NodeId,
) -> (String, String, Option<String>, Option<Uuid>, String) {
    sqlx::query_as(
        "SELECT flavor, ownership, lease_source, lease_request_id, region
           FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(pool)
    .await
    .expect("row must exist")
}

#[tokio::test]
async fn create_writes_rented_fanout_rows_with_a_deadline() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping create_writes_rented_fanout_rows_with_a_deadline");
        return;
    };

    let ids = lease_db::create(&pool, &req(Uuid::new_v4(), 3))
        .await
        .expect("create");
    assert_eq!(ids.len(), 3);

    for id in &ids {
        let (flavor, ownership, lease_source, lease_request_id, region) = row(&pool, id).await;
        assert_eq!(flavor, "fanout");
        assert_eq!(ownership, "rented");
        assert_eq!(lease_source.as_deref(), Some("manual"));
        assert!(lease_request_id.is_some());
        assert_eq!(region, "eu-ams");

        let deadline: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1",
        )
        .bind(id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            deadline.is_some(),
            "a rented row written without a deadline would violate the desired-set invariant"
        );
    }
}

/// Review Focus 4: two `create` calls with the same `request_id` return the same rows, and the
/// second call rents nothing new.
#[tokio::test]
async fn replayed_request_id_returns_the_same_rows_and_rents_nothing() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping replayed_request_id_returns_the_same_rows_and_rents_nothing"
        );
        return;
    };

    let request_id = Uuid::new_v4();
    let first = lease_db::create(&pool, &req(request_id, 2))
        .await
        .expect("first create");
    let second = lease_db::create(&pool, &req(request_id, 2))
        .await
        .expect("replayed create");

    assert_eq!(first, second, "a replay must return the same set of rows");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 2,
        "the replay must not have written a second set of rows"
    );
}

#[tokio::test]
async fn count_outside_one_to_five_is_refused_before_any_write() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping count_outside_one_to_five_is_refused_before_any_write");
        return;
    };

    for bad in [0u8, 6] {
        let err = lease_db::create(&pool, &req(Uuid::new_v4(), bad))
            .await
            .expect_err("an out-of-range count must be refused");
        assert!(matches!(err, LeaseError::InvalidCount(c) if c == bad));
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a refused create must write nothing");
}

/// Task 10 (carried Task 7 ruling (b)): `duration_hint` sizes the deadline at creation, and is
/// bounds-checked outside `MIN_DURATION_HINT`..=`MAX_DURATION_HINT`.
#[tokio::test]
async fn duration_hint_sizes_the_deadline_and_is_bounds_checked() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping duration_hint_sizes_the_deadline_and_is_bounds_checked"
        );
        return;
    };

    let mut custom = req(Uuid::new_v4(), 1);
    custom.duration_hint = Some(chrono::Duration::hours(4));
    let before = chrono::Utc::now();
    let ids = lease_db::create(&pool, &custom).await.expect("create");
    let deadline: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(ids[0].as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    let delta = deadline - before;
    assert!(
        delta > chrono::Duration::hours(3) + chrono::Duration::minutes(55)
            && delta < chrono::Duration::hours(4) + chrono::Duration::minutes(5),
        "deadline should be ~4h out, got {delta}"
    );

    for bad in [
        chrono::Duration::minutes(1),
        chrono::Duration::hours(25),
    ] {
        let mut req_bad = req(Uuid::new_v4(), 1);
        req_bad.duration_hint = Some(bad);
        let err = lease_db::create(&pool, &req_bad)
            .await
            .expect_err("an out-of-bounds duration_hint must be refused");
        assert!(matches!(err, LeaseError::InvalidDurationHint { .. }), "got: {err}");
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "the refused creates must write nothing");
}

async fn seed_node(pool: &PgPool, id: &str, state: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline)
         VALUES ($1, 'fanout', 'rented', 'dry-run', $2, now() + interval '1 hour')",
    )
    .bind(id)
    .bind(state)
    .execute(pool)
    .await
    .expect("seed node");
}

async fn node_state(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read node state")
}

/// A replayed request id asking for a DIFFERENT count is a different request wearing the same
/// id: refused, and nothing written (neither a top-up nor a partial match).
#[tokio::test]
async fn replayed_request_id_with_a_different_count_is_refused() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping replayed_request_id_with_a_different_count_is_refused"
        );
        return;
    };

    let request_id = Uuid::new_v4();
    lease_db::create(&pool, &req(request_id, 2))
        .await
        .expect("first create");
    let err = lease_db::create(&pool, &req(request_id, 3))
        .await
        .expect_err("a replay with a different count must be refused");
    assert!(
        matches!(err, LeaseError::CountMismatch { existing: 2, asked: 3, .. }),
        "got: {err}"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "the refused replay must not have topped the lease up");
}

#[tokio::test]
async fn release_of_a_missing_or_gone_node_is_a_typed_refusal() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_of_a_missing_or_gone_node_is_a_typed_refusal");
        return;
    };

    let missing = NodeId::new("lease-no-such-node");
    let err = lease_db::release(&pool, &missing, false)
        .await
        .expect_err("a node with no row must be NotFound");
    assert!(matches!(err, LeaseError::NotFound(_)), "got: {err}");

    let id = NodeId::new("lease-already-destroying");
    seed_node(&pool, id.as_str(), "destroying").await;
    let err = lease_db::release(&pool, &id, true)
        .await
        .expect_err("a destroying node must be AlreadyGone");
    assert!(
        matches!(err, LeaseError::AlreadyGone { ref state, .. } if state == "destroying"),
        "got: {err}"
    );
}

async fn lease_release_of(pool: &PgPool, id: &NodeId) -> Option<String> {
    sqlx::query_scalar("SELECT lease_release FROM mm_fleet_desired WHERE mm_node_id = $1")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .expect("desired row must exist")
}

#[tokio::test]
async fn release_marks_draining_without_forcing() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_marks_draining_without_forcing");
        return;
    };

    let ids = lease_db::create(&pool, &req(Uuid::new_v4(), 1)).await.expect("create");
    let id = &ids[0];
    seed_node(&pool, id.as_str(), "healthy").await;

    let state = lease_db::release(&pool, id, false).await.expect("release");
    assert_eq!(state, ReleaseState::Draining);
    assert_eq!(node_state(&pool, id.as_str()).await, "draining");
    assert_eq!(lease_release_of(&pool, id).await.as_deref(), Some("drain"));
}

/// A pool relay is not drained one at a time: a drained relay leaves the pool while its lease
/// stays paid, and the pool rents a replacement on top. Releasing one without force is
/// refused and changes nothing; with force it is evicted as any lease.
#[tokio::test]
async fn a_pool_lease_is_released_only_with_force() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_pool_lease_is_released_only_with_force");
        return;
    };
    let now = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    let id = &ids[0];
    seed_node(&pool, id.as_str(), "healthy").await;
    set_viewers(&pool, id, 3).await;
    let deadline = node_deadline(&pool, id).await;

    let err = lease_db::release(&pool, id, false)
        .await
        .expect_err("a pool lease is not drained");
    assert!(
        matches!(err, LeaseError::InSharedPool(ref n) if n == id),
        "got: {err}"
    );
    assert_eq!(
        err.to_string(),
        "this relay belongs to the shared pool: lower fleet.max_relay_nodes to shrink the pool, or release it with force"
    );
    assert_eq!(node_state(&pool, id.as_str()).await, "healthy");
    assert_eq!(lease_release_of(&pool, id).await, None);
    assert_eq!(node_deadline(&pool, id).await, deadline);

    let state = lease_db::release(&pool, id, true).await.expect("forced");
    assert_eq!(state, ReleaseState::Evicted);
    assert_eq!(node_state(&pool, id.as_str()).await, "draining");
    assert_eq!(lease_release_of(&pool, id).await.as_deref(), Some("force"));
}

#[tokio::test]
async fn forced_release_pulls_the_deadline_to_now_and_stays_evicted() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping forced_release_pulls_the_deadline_to_now_and_stays_evicted"
        );
        return;
    };

    let ids = lease_db::create(&pool, &req(Uuid::new_v4(), 1)).await.expect("create");
    let id = &ids[0];
    seed_node(&pool, id.as_str(), "healthy").await;
    // Viewers attached: exactly the case that would keep a drained node alive — a FORCED
    // release must evict it anyway.
    sqlx::query("UPDATE mm_fleet_nodes SET viewers_current = 7 WHERE mm_node_id = $1")
        .bind(id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let state = lease_db::release(&pool, id, true).await.expect("release");
    assert_eq!(state, ReleaseState::Evicted);
    assert_eq!(node_state(&pool, id.as_str()).await, "draining");
    assert_eq!(lease_release_of(&pool, id).await.as_deref(), Some("force"));

    let deadline: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_nodes WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        deadline <= chrono::Utc::now(),
        "a forced release must pull the deadline to now, not leave it rolling"
    );

    // CRITICAL 2: the next extension pass must NOT undo the eviction, viewers or not.
    let extended = lease_db::extend_leases(&pool, chrono::Utc::now())
        .await
        .expect("extend after a forced release");
    assert!(
        extended.is_empty(),
        "a force-released lease must never be extended again"
    );
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_nodes WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, deadline, "the floored deadline must stand");

    // And a later non-forced release must not downgrade the marker back to 'drain'.
    let _ = lease_db::release(&pool, id, false).await.expect("re-release");
    assert_eq!(lease_release_of(&pool, id).await.as_deref(), Some("force"));
}

// ── autoscale pool leases ─────────────────────────────────────────────────────

/// Postgres keeps microseconds; a `now` with nanoseconds would never read back equal.
fn db_now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

async fn set_viewers(pool: &PgPool, id: &NodeId, viewers: i32) {
    sqlx::query("UPDATE mm_fleet_nodes SET viewers_current = $2 WHERE mm_node_id = $1")
        .bind(id.as_str())
        .bind(viewers)
        .execute(pool)
        .await
        .expect("set viewers");
}

/// Drain-release a pool lease the way a drain release marks a manual one: its node
/// `draining`, its row `lease_release = 'drain'`. [`lease_db::release`] refuses this for a pool
/// lease, but the pool's passes still honour a drain marker on its row, so it is written
/// directly.
async fn mark_drain_released(pool: &PgPool, id: &NodeId) {
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'draining' WHERE mm_node_id = $1")
        .bind(id.as_str())
        .execute(pool)
        .await
        .expect("drain the node");
    sqlx::query("UPDATE mm_fleet_desired SET lease_release = 'drain' WHERE mm_node_id = $1")
        .bind(id.as_str())
        .execute(pool)
        .await
        .expect("mark the drain release");
}

async fn desired_deadline(pool: &PgPool, id: &NodeId) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .expect("desired deadline")
}

async fn node_deadline(pool: &PgPool, id: &NodeId) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT destroy_deadline FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .expect("node deadline")
}

async fn desired_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_autoscale_lease_is_operator_funded_and_tied_to_no_stream() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping an_autoscale_lease_is_operator_funded_and_tied_to_no_stream"
        );
        return;
    };

    let now = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 2, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    assert_eq!(ids.len(), 2);

    for (i, id) in ids.iter().enumerate() {
        let (source, funding, ownership, broadcast, created_by, region, request_id, deadline): (
            Option<String>,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<Uuid>,
            DateTime<Utc>,
        ) = sqlx::query_as(
            "SELECT lease_source, funding, ownership, broadcast_id, created_by, region,
                    lease_request_id, destroy_deadline
               FROM mm_fleet_desired WHERE mm_node_id = $1",
        )
        .bind(id.as_str())
        .fetch_one(&pool)
        .await
        .expect("autoscale row");
        assert_eq!(source.as_deref(), Some("autoscale"));
        assert_eq!(funding, "operator");
        assert_eq!(ownership, "rented");
        assert_eq!(broadcast, None, "a pool lease is tied to no stream");
        assert_eq!(created_by.as_deref(), Some("media-path"));
        assert_eq!(region, "eu");
        let request_id = request_id.expect("one request id per call");
        assert_eq!(id.as_str(), format!("lease-{request_id}-{i}"));
        assert_eq!(deadline, now + Duration::hours(1));
    }
}

#[tokio::test]
async fn an_autoscale_create_writes_up_to_a_hundred_rows_whole_or_nothing() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping an_autoscale_create_writes_up_to_a_hundred_rows_whole_or_nothing"
        );
        return;
    };
    let now = db_now();

    let none = lease_db::create_autoscale(&pool, "eu", 0, Duration::hours(1), now)
        .await
        .expect("count 0 is not an error");
    assert!(none.is_empty());
    assert_eq!(desired_count(&pool).await, 0, "count 0 writes nothing");

    let err = lease_db::create_autoscale(&pool, "eu", 101, Duration::hours(1), now)
        .await
        .expect_err("more than 100 must be refused");
    assert!(matches!(err, LeaseError::InvalidCount(101)), "got: {err}");
    assert_eq!(
        desired_count(&pool).await,
        0,
        "a refused create writes nothing"
    );

    let hundred = lease_db::create_autoscale(&pool, "eu", 100, Duration::hours(1), now)
        .await
        .expect("100 is allowed: the manual limit of 5 does not apply");
    assert_eq!(hundred.len(), 100);
    assert_eq!(desired_count(&pool).await, 100);
}

#[tokio::test]
async fn the_live_pool_counts_a_lease_with_no_node_yet_as_idle_and_skips_released_ones() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping the_live_pool_counts_a_lease_with_no_node_yet_as_idle_and_skips_released_ones"
        );
        return;
    };
    let now = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 3, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    // ids[0]: no node row yet. ids[1]: rented, 4 viewers. ids[2]: released.
    seed_node(&pool, ids[1].as_str(), "healthy").await;
    set_viewers(&pool, &ids[1], 4).await;
    seed_node(&pool, ids[2].as_str(), "healthy").await;
    mark_drain_released(&pool, &ids[2]).await;
    // A manual lease is not part of the pool.
    lease_db::create(&pool, &req(Uuid::new_v4(), 1))
        .await
        .expect("manual");

    let live = lease_db::autoscale_live(&pool)
        .await
        .expect("autoscale_live");
    assert_eq!(
        live,
        vec![
            AutoscaleLease {
                node_id: ids[0].clone(),
                viewers_current: 0,
                destroy_deadline: now + Duration::hours(1),
            },
            AutoscaleLease {
                node_id: ids[1].clone(),
                viewers_current: 4,
                destroy_deadline: now + Duration::hours(1),
            },
        ]
    );
}

/// Three healthy leases with viewers 0, 0, 5.
async fn three_pool_leases(pool: &PgPool, now: DateTime<Utc>) -> Vec<NodeId> {
    let ids = lease_db::create_autoscale(pool, "eu", 3, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    seed_node(pool, ids[0].as_str(), "healthy").await;
    seed_node(pool, ids[1].as_str(), "healthy").await;
    seed_node(pool, ids[2].as_str(), "healthy").await;
    set_viewers(pool, &ids[2], 5).await;
    ids
}

#[tokio::test]
async fn extension_keeps_wanted_and_busy_leases_and_lets_the_rest_lapse() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping extension_keeps_wanted_and_busy_leases_and_lets_the_rest_lapse"
        );
        return;
    };
    let created = db_now();
    let ids = three_pool_leases(&pool, created).await;
    let idle_desired = desired_deadline(&pool, &ids[1]).await;
    let idle_node = node_deadline(&pool, &ids[1]).await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 2, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(
        extended,
        vec![ids[0].clone(), ids[2].clone()],
        "keep = 2: the busy lease and the lowest-id idle one"
    );
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        desired_deadline(&pool, &ids[2]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        node_deadline(&pool, &ids[2]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        desired_deadline(&pool, &ids[1]).await,
        idle_desired,
        "the unwanted idle lease is not touched"
    );
    assert_eq!(node_deadline(&pool, &ids[1]).await, idle_node);
}

#[tokio::test]
async fn keep_one_extends_only_the_busy_lease() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping keep_one_extends_only_the_busy_lease");
        return;
    };
    let created = db_now();
    let ids = three_pool_leases(&pool, created).await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(extended, vec![ids[2].clone()]);
    assert_eq!(
        desired_deadline(&pool, &ids[2]).await,
        now + Duration::hours(1)
    );
    for idle in &ids[..2] {
        assert_eq!(
            desired_deadline(&pool, idle).await,
            created + Duration::hours(1),
            "{idle} is idle and beyond keep"
        );
    }
}

#[tokio::test]
async fn a_lease_with_viewers_is_extended_even_with_keep_zero() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_lease_with_viewers_is_extended_even_with_keep_zero"
        );
        return;
    };
    let created = db_now();
    let ids = three_pool_leases(&pool, created).await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 0, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(extended, vec![ids[2].clone()]);
    assert_eq!(
        desired_deadline(&pool, &ids[2]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        node_deadline(&pool, &ids[2]).await,
        now + Duration::hours(1)
    );
    for idle in &ids[..2] {
        assert_eq!(
            desired_deadline(&pool, idle).await,
            created + Duration::hours(1)
        );
    }
}

#[tokio::test]
async fn a_force_released_autoscale_lease_is_not_extended_even_with_viewers() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_force_released_autoscale_lease_is_not_extended_even_with_viewers"
        );
        return;
    };
    let created = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    seed_node(&pool, ids[0].as_str(), "healthy").await;
    set_viewers(&pool, &ids[0], 3).await;
    lease_db::release(&pool, &ids[0], true)
        .await
        .expect("release");
    let floored = node_deadline(&pool, &ids[0]).await;

    let extended = lease_db::extend_autoscale(
        &pool,
        5,
        Duration::hours(1),
        created + Duration::minutes(30),
    )
    .await
    .expect("extend_autoscale");
    assert!(
        extended.is_empty(),
        "a forced release is never extended: {extended:?}"
    );
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        created + Duration::hours(1)
    );
    assert_eq!(node_deadline(&pool, &ids[0]).await, floored);
}

/// Leases: ids[0] drain-released, its node draining; ids[1] idle with a node; ids[2] no node.
async fn a_drained_lease_and_two_idle_ones(pool: &PgPool, now: DateTime<Utc>) -> Vec<NodeId> {
    let ids = lease_db::create_autoscale(pool, "eu", 3, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    seed_node(pool, ids[0].as_str(), "healthy").await;
    seed_node(pool, ids[1].as_str(), "healthy").await;
    mark_drain_released(pool, &ids[0]).await;
    ids
}

/// A drain-released lease keeps the manual R5 rule: extended while its viewers stay, and never
/// counted toward `keep` (the pool sees it as gone and orders a replacement).
#[tokio::test]
async fn a_drain_released_autoscale_lease_with_viewers_drains_outside_keep() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_drain_released_autoscale_lease_with_viewers_drains_outside_keep"
        );
        return;
    };
    let created = db_now();
    let ids = a_drained_lease_and_two_idle_ones(&pool, created).await;
    set_viewers(&pool, &ids[0], 5).await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(
        extended,
        vec![ids[0].clone(), ids[1].clone()],
        "the draining lease by the drain rule, and keep = 1 still buys one unreleased lease"
    );
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        node_deadline(&pool, &ids[0]).await,
        now + Duration::hours(1)
    );
    assert_eq!(
        desired_deadline(&pool, &ids[2]).await,
        created + Duration::hours(1)
    );
}

#[tokio::test]
async fn a_drain_released_autoscale_lease_without_viewers_is_not_extended() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_drain_released_autoscale_lease_without_viewers_is_not_extended"
        );
        return;
    };
    let created = db_now();
    let ids = a_drained_lease_and_two_idle_ones(&pool, created).await;

    let extended = lease_db::extend_autoscale(
        &pool,
        5,
        Duration::hours(1),
        created + Duration::minutes(30),
    )
    .await
    .expect("extend_autoscale");
    assert_eq!(
        extended,
        vec![ids[1].clone()],
        "an empty drained lease is not extended, however large keep is (and a lease with no \
         node row is not eligible)"
    );
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        created + Duration::hours(1)
    );
}

/// A lease whose node is being torn down is not extended whatever its count says, and does not
/// take a `keep` slot.
#[tokio::test]
async fn a_lease_whose_node_is_destroying_or_gone_is_not_extended() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_lease_whose_node_is_destroying_or_gone_is_not_extended"
        );
        return;
    };
    let created = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 3, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    seed_node(&pool, ids[0].as_str(), "destroying").await;
    set_viewers(&pool, &ids[0], 5).await;
    seed_node(&pool, ids[1].as_str(), "gone").await;
    set_viewers(&pool, &ids[1], 5).await;
    seed_node(&pool, ids[2].as_str(), "healthy").await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(
        extended,
        vec![ids[2].clone()],
        "only the lease with a live future takes the keep slot"
    );
    for going in &ids[..2] {
        assert_eq!(
            desired_deadline(&pool, going).await,
            created + Duration::hours(1)
        );
    }
}

// ── fix round 1: eligibility (finding 1) ──────────────────────────────────────

/// A node stuck in boot runs on its first deadline like a manual one, and takes no keep slot.
#[tokio::test]
async fn a_booting_lease_in_keep_is_not_extended_and_takes_no_slot() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_booting_lease_in_keep_is_not_extended_and_takes_no_slot"
        );
        return;
    };
    let created = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 2, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    seed_node(&pool, ids[0].as_str(), "booting").await;
    seed_node(&pool, ids[1].as_str(), "healthy").await;

    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(extended, vec![ids[1].clone()]);
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        created + Duration::hours(1),
        "the booting lease keeps the deadline it was created with"
    );
}

#[tokio::test]
async fn a_lease_with_no_node_row_is_not_extended() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping a_lease_with_no_node_row_is_not_extended");
        return;
    };
    let created = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), created)
        .await
        .expect("create_autoscale");

    let extended = lease_db::extend_autoscale(
        &pool,
        1,
        Duration::hours(1),
        created + Duration::minutes(30),
    )
    .await
    .expect("extend_autoscale");
    assert!(extended.is_empty(), "{extended:?}");
    assert_eq!(
        desired_deadline(&pool, &ids[0]).await,
        created + Duration::hours(1)
    );
}

#[tokio::test]
async fn equal_busy_leases_are_all_extended_and_equal_idle_ones_go_by_id() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping equal_busy_leases_are_all_extended_and_equal_idle_ones_go_by_id"
        );
        return;
    };
    let created = db_now();
    let ids = lease_db::create_autoscale(&pool, "eu", 2, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    for id in &ids {
        seed_node(&pool, id.as_str(), "healthy").await;
        set_viewers(&pool, id, 3).await;
    }
    let now = created + Duration::minutes(30);
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(
        extended, ids,
        "both are busy: keep does not limit busy leases"
    );

    for id in &ids {
        set_viewers(&pool, id, 0).await;
    }
    let extended = lease_db::extend_autoscale(&pool, 1, Duration::hours(1), now)
        .await
        .expect("extend_autoscale");
    assert_eq!(
        extended,
        vec![ids[0].clone()],
        "a tie at zero goes to the lower id"
    );
}

// ── fix round 1: sizing the pool (finding 2) ──────────────────────────────────

async fn autoscale_ids(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT mm_node_id FROM mm_fleet_desired WHERE lease_source = 'autoscale' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn concurrent_top_ups_create_the_shortfall_once() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping concurrent_top_ups_create_the_shortfall_once"
        );
        return;
    };
    let now = db_now();
    let (a, b) = tokio::join!(
        lease_db::top_up_autoscale(&pool, "eu", 2, Duration::hours(1), now),
        lease_db::top_up_autoscale(&pool, "eu", 2, Duration::hours(1), now),
    );
    let mut created: Vec<String> = a
        .expect("first top-up")
        .into_iter()
        .chain(b.expect("second top-up"))
        .map(|id| id.as_str().to_string())
        .collect();
    created.sort();
    let rows = autoscale_ids(&pool).await;
    assert_eq!(
        rows.len(),
        2,
        "two concurrent calls must not both fill the gap: {rows:?}"
    );
    assert_eq!(
        created, rows,
        "together the calls name exactly the rows that exist"
    );

    let again = lease_db::top_up_autoscale(&pool, "eu", 2, Duration::hours(1), now)
        .await
        .expect("third top-up");
    assert!(again.is_empty(), "a full pool creates nothing: {again:?}");
    assert_eq!(autoscale_ids(&pool).await.len(), 2);
}

#[tokio::test]
async fn a_top_up_creates_only_the_shortfall_and_ignores_gone_leases() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_top_up_creates_only_the_shortfall_and_ignores_gone_leases"
        );
        return;
    };
    let now = db_now();
    lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("one live lease");
    let created = lease_db::top_up_autoscale(&pool, "eu", 3, Duration::hours(1), now)
        .await
        .expect("top up to 3");
    assert_eq!(created.len(), 2);
    assert_eq!(autoscale_ids(&pool).await.len(), 3);

    sqlx::query("DELETE FROM mm_fleet_desired")
        .execute(&pool)
        .await
        .unwrap();
    let gone = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("a lease");
    seed_node(&pool, gone[0].as_str(), "gone").await;
    assert!(
        lease_db::autoscale_live(&pool)
            .await
            .expect("live")
            .is_empty(),
        "a lease whose node is gone is not live"
    );
    let created = lease_db::top_up_autoscale(&pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("top up to 1");
    assert_eq!(created.len(), 1, "the gone lease is not counted");
}

#[tokio::test]
async fn a_top_up_writes_at_most_a_hundred_rows_per_call() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_top_up_writes_at_most_a_hundred_rows_per_call"
        );
        return;
    };
    let created = lease_db::top_up_autoscale(&pool, "eu", 250, Duration::hours(1), db_now())
        .await
        .expect("top up to 250");
    assert_eq!(created.len(), 100);
    assert_eq!(autoscale_ids(&pool).await.len(), 100);
}

// ── fix round 1: retiring idle leases (finding 3) ─────────────────────────────

fn deadline_kills() -> u64 {
    mm_core::metrics_global::FLEET_REAPER_DEADLINE_KILLS
        .with_label_values(&["fanout"])
        .get()
}

async fn set_deadlines(pool: &PgPool, id: &NodeId, deadline: DateTime<Utc>) {
    for t in ["mm_fleet_desired", "mm_fleet_nodes"] {
        sqlx::query(&format!(
            "UPDATE {t} SET destroy_deadline = $2 WHERE mm_node_id = $1"
        ))
        .bind(id.as_str())
        .bind(deadline)
        .execute(pool)
        .await
        .expect("set deadline");
    }
}

/// One rented autoscale lease in `state`, both deadlines `until` after `now`.
async fn pool_lease_due_in(
    pool: &PgPool,
    now: DateTime<Utc>,
    state: &str,
    until: Duration,
) -> NodeId {
    let ids = lease_db::create_autoscale(pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    seed_node(pool, ids[0].as_str(), state).await;
    set_deadlines(pool, &ids[0], now + until).await;
    ids[0].clone()
}

async fn desired_exists(pool: &PgPool, id: &NodeId) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mm_fleet_desired WHERE mm_node_id = $1)")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_idle_lease_about_to_lapse_is_retired_without_a_deadline_kill() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping an_idle_lease_about_to_lapse_is_retired_without_a_deadline_kill"
        );
        return;
    };
    let now = db_now();
    let id = pool_lease_due_in(&pool, now, "healthy", Duration::minutes(2)).await;
    let kills = deadline_kills();

    let retired = lease_db::retire_idle_autoscale(&pool, Duration::minutes(3), now)
        .await
        .expect("retire");
    assert_eq!(retired, vec![id.clone()]);
    assert!(
        !desired_exists(&pool, &id).await,
        "the desired row is withdrawn"
    );
    assert_eq!(node_state(&pool, id.as_str()).await, "destroying");
    assert_eq!(
        deadline_kills(),
        kills,
        "an ordered teardown is not a deadline kill"
    );
}

#[tokio::test]
async fn busy_distant_booting_forced_and_manual_leases_are_not_retired() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping busy_distant_booting_forced_and_manual_leases_are_not_retired"
        );
        return;
    };
    let now = db_now();
    let busy = pool_lease_due_in(&pool, now, "healthy", Duration::minutes(2)).await;
    set_viewers(&pool, &busy, 1).await;
    let distant = pool_lease_due_in(&pool, now, "healthy", Duration::minutes(10)).await;
    let booting = pool_lease_due_in(&pool, now, "booting", Duration::minutes(2)).await;
    let forced = pool_lease_due_in(&pool, now, "healthy", Duration::minutes(2)).await;
    lease_db::release(&pool, &forced, true)
        .await
        .expect("force release");
    let mut manual_req = req(Uuid::new_v4(), 1);
    manual_req.duration_hint = Some(Duration::minutes(15));
    let manual = lease_db::create(&pool, &manual_req).await.expect("manual")[0].clone();
    seed_node(&pool, manual.as_str(), "healthy").await;
    set_deadlines(&pool, &manual, now + Duration::minutes(2)).await;

    let retired = lease_db::retire_idle_autoscale(&pool, Duration::minutes(3), now)
        .await
        .expect("retire");
    assert!(retired.is_empty(), "{retired:?}");
    for id in [&busy, &distant, &booting, &forced, &manual] {
        assert!(
            desired_exists(&pool, id).await,
            "{id} must keep its desired row"
        );
        assert_ne!(node_state(&pool, id.as_str()).await, "destroying", "{id}");
    }
}

#[tokio::test]
async fn manual_extension_does_not_touch_autoscale_leases() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping manual_extension_does_not_touch_autoscale_leases"
        );
        return;
    };
    let created = db_now();
    let pool_ids = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    seed_node(&pool, pool_ids[0].as_str(), "healthy").await;
    // A drain-released autoscale lease with viewers is exactly what the manual rule would
    // extend; the manual pass must still leave it to `extend_autoscale`.
    let drained = lease_db::create_autoscale(&pool, "eu", 1, Duration::hours(1), created)
        .await
        .expect("create_autoscale");
    seed_node(&pool, drained[0].as_str(), "healthy").await;
    set_viewers(&pool, &drained[0], 4).await;
    mark_drain_released(&pool, &drained[0]).await;
    let manual = lease_db::create(&pool, &req(Uuid::new_v4(), 1))
        .await
        .expect("manual");
    seed_node(&pool, manual[0].as_str(), "healthy").await;

    let extended = lease_db::extend_leases(&pool, created + Duration::minutes(30))
        .await
        .expect("extend_leases");
    assert_eq!(extended, vec![manual[0].clone()], "only the manual lease");
    assert_eq!(
        desired_deadline(&pool, &pool_ids[0]).await,
        created + Duration::hours(1),
        "the autoscale lease's deadline is not moved by the manual pass"
    );
}

#[tokio::test]
async fn extend_leases_rolls_the_deadline_for_healthy_manual_leases_only() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping extend_leases_rolls_the_deadline_for_healthy_manual_leases_only"
        );
        return;
    };

    let ids = lease_db::create(&pool, &req(Uuid::new_v4(), 1))
        .await
        .expect("create");
    let id = &ids[0];
    // The node row does not exist yet (nothing has rented it) — extend_leases must skip it
    // cleanly rather than erroring.
    let extended = lease_db::extend_leases(&pool, chrono::Utc::now())
        .await
        .expect("extend with no node row yet");
    assert!(extended.is_empty());

    seed_node(&pool, id.as_str(), "healthy").await;
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();

    let now = chrono::Utc::now() + chrono::Duration::minutes(50);
    let extended = lease_db::extend_leases(&pool, now).await.expect("extend");
    assert_eq!(extended, vec![id.clone()]);

    let after_desired: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    let after_node: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_nodes WHERE mm_node_id = $1",
    )
    .bind(id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(after_desired > before, "the deadline must have rolled forward");
    assert_eq!(
        after_desired, after_node,
        "the desired row and the node row must agree on the deadline"
    );
}
