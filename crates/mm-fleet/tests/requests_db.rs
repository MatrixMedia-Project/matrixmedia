use std::sync::OnceLock;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{self as pdb, ProviderInput};
use mm_fleet::requests_db::{self as rq, NewRequest};
use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never
/// land inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(sqlx::PgPool, String, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in [
        "mm_fleet_requests",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_providers",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    let id = pdb::insert(
        &pool,
        &ProviderInput {
            label: "A".into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: None,
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: None,
            max_gpu_nodes: 1,
            zones: vec![],
        },
    )
    .await
    .unwrap();
    Some((pool, id, guard))
}

#[tokio::test]
async fn claim_is_exclusive_and_in_order() {
    let Some((pool, p, _g)) = setup().await else { return; };
    let a = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@argi:x" }).await.unwrap();
    let b = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: Some("again"), requested_by: "@argi:x" }).await.unwrap();
    let first = rq::claim_next(&pool).await.unwrap().expect("one");
    assert_eq!(first.id, a);
    assert_eq!(first.state, "running");
    let second = rq::claim_next(&pool).await.unwrap().expect("two");
    assert_eq!(second.id, b);
    assert!(rq::claim_next(&pool).await.unwrap().is_none());
    rq::finish(&pool, &a, true, json!({"state": "ok"})).await.unwrap();
    let row = rq::get(&pool, &a).await.unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert!(row.finished_at.is_some());
    assert_eq!(row.result.unwrap()["state"], "ok");
}

#[tokio::test]
async fn an_unclaimed_request_older_than_the_ttl_expires_and_is_never_claimed() {
    let Some((pool, p, _g)) = setup().await else { return; };
    let a = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@argi:x" }).await.unwrap();
    sqlx::query("UPDATE mm_fleet_requests SET expires_at = now() - interval '1 second' WHERE id = $1").bind(&a).execute(&pool).await.unwrap();
    assert!(rq::claim_next(&pool).await.unwrap().is_none(), "expired rows are not claimable even before expire_stale runs");
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 1);
    assert_eq!(rq::get(&pool, &a).await.unwrap().unwrap().state, "expired");
}

#[tokio::test]
async fn queued_count_per_provider_and_kind() {
    let Some((pool, p, _g)) = setup().await else { return; };
    rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@a:x" }).await.unwrap();
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 1);
    assert_eq!(rq::count_queued_for(&pool, &p, "test_boot").await.unwrap(), 0);
}

#[tokio::test]
async fn a_claim_older_than_the_ttl_is_a_dead_runner_and_expires() {
    let Some((pool, p, _g)) = setup().await else { return; };
    let a = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@argi:x" }).await.unwrap();
    assert_eq!(rq::claim_next(&pool).await.unwrap().expect("claimed").id, a);
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 1, "a fresh claim is pending");
    sqlx::query("UPDATE mm_fleet_requests SET claimed_at = now() - interval '301 seconds' WHERE id = $1").bind(&a).execute(&pool).await.unwrap();
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 0, "a dead runner's claim is not pending, even before expire_stale runs");
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 1);
    let row = rq::get(&pool, &a).await.unwrap().unwrap();
    assert_eq!(row.state, "expired");
    assert!(row.finished_at.is_some());
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 0, "already expired rows are not expired again");
}

#[tokio::test]
async fn a_fresh_claim_is_not_expired_and_still_counts() {
    let Some((pool, p, _g)) = setup().await else { return; };
    let a = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@argi:x" }).await.unwrap();
    assert_eq!(rq::claim_next(&pool).await.unwrap().expect("claimed").id, a);
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 0);
    assert_eq!(rq::get(&pool, &a).await.unwrap().unwrap().state, "running");
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 1);
    rq::finish(&pool, &a, true, json!({"state": "ok"})).await.unwrap();
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 0, "a finished request is no longer pending");
}

#[tokio::test]
async fn an_expired_but_unswept_queued_request_does_not_count() {
    let Some((pool, p, _g)) = setup().await else { return; };
    let a = rq::enqueue(&pool, &NewRequest { kind: "test_connection", provider_id: &p, zone: None, role: None, reason: None, requested_by: "@argi:x" }).await.unwrap();
    sqlx::query("UPDATE mm_fleet_requests SET expires_at = now() - interval '1 second' WHERE id = $1").bind(&a).execute(&pool).await.unwrap();
    assert_eq!(rq::count_queued_for(&pool, &p, "test_connection").await.unwrap(), 0);
}
