//! PG-gated coverage for manual relay leases (Task 7).
//!
//! The pure extension decision (`should_extend`) has its own table tests in
//! `mm-fleet/src/lease_db.rs`; what needs a database is the idempotent create (Review Focus 4)
//! and the release state transitions.

use std::sync::OnceLock;

use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::lease_db::{self, LeaseCreate, LeaseError, ReleaseState};
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
