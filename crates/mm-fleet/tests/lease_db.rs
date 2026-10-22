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

#[tokio::test]
async fn release_marks_draining_without_forcing() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_marks_draining_without_forcing");
        return;
    };

    let id = NodeId::new("lease-release-1");
    seed_node(&pool, id.as_str(), "healthy").await;

    let state = lease_db::release(&pool, &id, false).await.expect("release");
    assert_eq!(state, ReleaseState::Draining);
    assert_eq!(node_state(&pool, id.as_str()).await, "draining");
}

#[tokio::test]
async fn forced_release_pulls_the_deadline_to_now() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping forced_release_pulls_the_deadline_to_now");
        return;
    };

    let id = NodeId::new("lease-release-2");
    seed_node(&pool, id.as_str(), "healthy").await;

    let state = lease_db::release(&pool, &id, true).await.expect("release");
    assert_eq!(state, ReleaseState::Evicted);
    assert_eq!(node_state(&pool, id.as_str()).await, "draining");

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
