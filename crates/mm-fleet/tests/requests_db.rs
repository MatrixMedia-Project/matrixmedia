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
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let a = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    let b = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: Some("again"),
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    let first = rq::claim_next(&pool, "test_connection")
        .await
        .unwrap()
        .expect("one");
    assert_eq!(first.id, a);
    assert_eq!(first.state, "running");
    let second = rq::claim_next(&pool, "test_connection")
        .await
        .unwrap()
        .expect("two");
    assert_eq!(second.id, b);
    assert!(
        rq::claim_next(&pool, "test_connection")
            .await
            .unwrap()
            .is_none()
    );
    rq::finish(&pool, &a, true, json!({"state": "ok"}))
        .await
        .unwrap();
    let row = rq::get(&pool, &a).await.unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert!(row.finished_at.is_some());
    assert_eq!(row.result.unwrap()["state"], "ok");
}

#[tokio::test]
async fn an_unclaimed_request_older_than_the_ttl_expires_and_is_never_claimed() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let a = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_requests SET expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(&a)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        rq::claim_next(&pool, "test_connection")
            .await
            .unwrap()
            .is_none(),
        "expired rows are not claimable even before expire_stale runs"
    );
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 1);
    assert_eq!(rq::get(&pool, &a).await.unwrap().unwrap().state, "expired");
}

#[tokio::test]
async fn queued_count_per_provider_and_kind() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@a:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_boot").await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_claim_older_than_the_ttl_is_a_dead_runner_and_expires() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let a = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rq::claim_next(&pool, "test_connection")
            .await
            .unwrap()
            .expect("claimed")
            .id,
        a
    );
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        1,
        "a fresh claim is pending"
    );
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '301 seconds' WHERE id = $1",
    )
    .bind(&a)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        0,
        "a dead runner's claim is not pending, even before expire_stale runs"
    );
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 1);
    let row = rq::get(&pool, &a).await.unwrap().unwrap();
    assert_eq!(row.state, "expired");
    assert!(row.finished_at.is_some());
    assert_eq!(
        rq::expire_stale(&pool).await.unwrap(),
        0,
        "already expired rows are not expired again"
    );
}

#[tokio::test]
async fn a_fresh_claim_is_not_expired_and_still_counts() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let a = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rq::claim_next(&pool, "test_connection")
            .await
            .unwrap()
            .expect("claimed")
            .id,
        a
    );
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 0);
    assert_eq!(rq::get(&pool, &a).await.unwrap().unwrap().state, "running");
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        1
    );
    rq::finish(&pool, &a, true, json!({"state": "ok"}))
        .await
        .unwrap();
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        0,
        "a finished request is no longer pending"
    );
}

#[tokio::test]
async fn an_expired_but_unswept_queued_request_does_not_count() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let a = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_requests SET expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(&a)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_connection")
            .await
            .unwrap(),
        0
    );
}

fn boot_request<'a>(provider_id: &'a str, params: serde_json::Value) -> NewRequest<'a> {
    NewRequest {
        kind: "test_boot",
        provider_id,
        zone: Some("z"),
        role: Some("transcode"),
        reason: Some("prove it"),
        requested_by: "@argi:example",
        params,
    }
}

#[tokio::test]
async fn claims_are_per_kind_and_test_boots_keep_their_claim_longer() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let boot = rq::enqueue(
        &pool,
        &boot_request(&p, json!({"report_url": "https://x/r"})),
    )
    .await
    .unwrap();
    assert!(
        rq::claim_next(&pool, "test_connection")
            .await
            .unwrap()
            .is_none(),
        "a test boot is not a test connection"
    );
    let claimed = rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(claimed.id, boot);
    assert_eq!(claimed.params["report_url"], "https://x/r");
    // Ten minutes into a boot is normal, not a dead runner.
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '10 minutes' WHERE id = $1",
    )
    .bind(&boot)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 0);
    assert_eq!(rq::live_count(&pool, "test_boot").await.unwrap(), 1);
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_boot").await.unwrap(),
        1,
        "a test boot's claim is alive at ten minutes"
    );
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '26 minutes' WHERE id = $1",
    )
    .bind(&boot)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        rq::count_queued_for(&pool, &p, "test_boot").await.unwrap(),
        0,
        "a dead runner's claim is not pending before the sweep either"
    );
    assert_eq!(rq::live_count(&pool, "test_boot").await.unwrap(), 0);
    assert_eq!(rq::expire_stale(&pool).await.unwrap(), 1);
    assert_eq!(
        rq::get(&pool, &boot).await.unwrap().unwrap().state,
        "expired"
    );
}

#[tokio::test]
async fn a_test_connection_claim_still_dies_after_five_minutes_beside_a_live_test_boot() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let conn = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    let boot = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    rq::claim_next(&pool, "test_connection")
        .await
        .unwrap()
        .expect("connection claimed");
    rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("boot claimed");
    sqlx::query(
        "UPDATE mm_fleet_requests SET claimed_at = now() - interval '6 minutes' WHERE id = ANY($1)",
    )
    .bind(vec![conn.clone(), boot.clone()])
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        rq::expire_stale(&pool).await.unwrap(),
        1,
        "only the test connection's claim is dead"
    );
    assert_eq!(
        rq::get(&pool, &conn).await.unwrap().unwrap().state,
        "expired"
    );
    assert_eq!(
        rq::get(&pool, &boot).await.unwrap().unwrap().state,
        "running"
    );
}

#[tokio::test]
async fn progress_and_finish_merge_into_the_result() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let id = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_boot",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:example",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    assert!(
        !rq::progress(&pool, &id, json!({"phase": "creating"}))
            .await
            .unwrap(),
        "only a running request takes progress"
    );
    rq::claim_next(&pool, "test_boot").await.unwrap();
    assert!(
        rq::progress(&pool, &id, json!({"phase": "creating", "node_id": "tb-1"}))
            .await
            .unwrap()
    );
    assert!(
        rq::progress(
            &pool,
            &id,
            json!({"phase": "booting", "released_by": "@argi:example"})
        )
        .await
        .unwrap()
    );
    rq::finish(&pool, &id, true, json!({"phase": "done", "nvenc": "ok"}))
        .await
        .unwrap();
    let r = rq::get(&pool, &id).await.unwrap().unwrap();
    assert_eq!(r.state, "done");
    assert_eq!(
        r.result.unwrap(),
        json!({"phase": "done", "node_id": "tb-1", "released_by": "@argi:example", "nvenc": "ok"})
    );
    assert!(
        !rq::progress(&pool, &id, json!({"phase": "late"}))
            .await
            .unwrap(),
        "a finished request takes no more progress"
    );
}

#[tokio::test]
async fn queued_test_boots_can_be_failed_in_one_go_and_count_today() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    for _ in 0..2 {
        rq::enqueue(
            &pool,
            &NewRequest {
                kind: "test_boot",
                provider_id: &p,
                zone: None,
                role: None,
                reason: None,
                requested_by: "@argi:example",
                params: json!({}),
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(rq::count_today(&pool, "test_boot").await.unwrap(), 2);
    assert_eq!(
        rq::fail_queued(&pool, "test_boot", "fleet.mode is off")
            .await
            .unwrap(),
        2
    );
    assert!(rq::running(&pool, "test_boot").await.unwrap().is_empty());
    assert_eq!(
        rq::count_today(&pool, "test_boot").await.unwrap(),
        2,
        "a refused boot still counts toward the day"
    );
    let failed = rq::claim_next(&pool, "test_boot").await.unwrap();
    assert!(failed.is_none(), "a refused boot is not claimable");
}

#[tokio::test]
async fn failing_the_queue_leaves_other_kinds_and_running_requests_alone() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let running = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("claimed");
    let queued = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    let conn = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &p,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rq::fail_queued(&pool, "test_boot", "fleet.mode is off")
            .await
            .unwrap(),
        1
    );
    let q = rq::get(&pool, &queued).await.unwrap().unwrap();
    assert_eq!(q.state, "failed");
    assert_eq!(q.result.unwrap(), json!({"error": "fleet.mode is off"}));
    assert!(q.finished_at.is_some());
    assert_eq!(
        rq::get(&pool, &running).await.unwrap().unwrap().state,
        "running",
        "a running boot is the runner's to finish"
    );
    assert_eq!(
        rq::get(&pool, &conn).await.unwrap().unwrap().state,
        "queued",
        "another kind is not touched"
    );
    let still: Vec<String> = rq::running(&pool, "test_boot")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(still, vec![running]);
    assert!(
        rq::running(&pool, "test_connection")
            .await
            .unwrap()
            .is_empty(),
        "running is per kind"
    );
}

#[tokio::test]
async fn count_today_is_the_utc_day_and_per_kind() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let old = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_requests SET requested_at = (date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') - interval '1 second' WHERE id = $1")
        .bind(&old).execute(&pool).await.unwrap();
    assert_eq!(
        rq::count_today(&pool, "test_boot").await.unwrap(),
        0,
        "one second before UTC midnight is yesterday"
    );
    let first = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_requests SET requested_at = (date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') WHERE id = $1")
        .bind(&first).execute(&pool).await.unwrap();
    assert_eq!(
        rq::count_today(&pool, "test_boot").await.unwrap(),
        1,
        "UTC midnight itself is today"
    );
    assert_eq!(rq::count_today(&pool, "test_connection").await.unwrap(), 0);
}

/// The oldest request goes first by `requested_at`, not by where its row sits in the heap: the
/// first one inserted is made the newest.
#[tokio::test]
async fn claims_and_the_running_list_go_oldest_first_whatever_the_row_order() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let inserted_first = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    let inserted_second = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_requests SET requested_at = now() + interval '1 minute' WHERE id = $1",
    )
    .bind(&inserted_first)
    .execute(&pool)
    .await
    .unwrap();
    let a = rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("first claim");
    let b = rq::claim_next(&pool, "test_boot")
        .await
        .unwrap()
        .expect("second claim");
    assert_eq!(
        (a.id.as_str(), b.id.as_str()),
        (inserted_second.as_str(), inserted_first.as_str())
    );
    let running: Vec<String> = rq::running(&pool, "test_boot")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(running, vec![inserted_second, inserted_first]);
}

#[tokio::test]
async fn the_merge_is_shallow_a_nested_object_is_replaced_whole() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let id = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    rq::claim_next(&pool, "test_boot").await.unwrap();
    rq::progress(
        &pool,
        &id,
        json!({"phase": "booting", "timings": {"create": 1, "boot": 2}}),
    )
    .await
    .unwrap();
    rq::progress(&pool, &id, json!({"timings": {"boot": 3}}))
        .await
        .unwrap();
    rq::finish(&pool, &id, true, json!({"phase": "done"}))
        .await
        .unwrap();
    assert_eq!(
        rq::get(&pool, &id).await.unwrap().unwrap().result.unwrap(),
        json!({"phase": "done", "timings": {"boot": 3}})
    );
}

/// "Today" is the UTC day whatever time zone the session runs in. Kiritimati (UTC+14) is the
/// zone furthest from UTC, so a day taken in the session's zone starts 14 hours away from the
/// UTC one: a row just before UTC midnight and one at it land on opposite sides of the line
/// only for the UTC definition.
#[tokio::test]
async fn count_today_is_the_utc_day_in_a_session_in_any_time_zone() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    // The date comes from the database's clock (the host's may be minutes off it); the
    // midnight is built here, in Rust, not with the SQL under test.
    let today: chrono::NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC')::date")
        .fetch_one(&pool)
        .await
        .unwrap();
    let midnight = today.and_hms_opt(0, 0, 0).unwrap().and_utc();
    for at in [midnight - chrono::Duration::seconds(1), midnight] {
        let id = rq::enqueue(&pool, &boot_request(&p, json!({})))
            .await
            .unwrap();
        sqlx::query("UPDATE mm_fleet_requests SET requested_at = $2 WHERE id = $1")
            .bind(&id)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL TIME ZONE 'Pacific/Kiritimati'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        rq::count_today(&mut *tx, "test_boot").await.unwrap(),
        1,
        "the row at UTC midnight is today's, the one a second before is yesterday's"
    );
    tx.rollback().await.unwrap();
}

/// A non-object would not merge: Postgres turns `result || '5'` into an array. Debug builds
/// refuse it at the call.
#[cfg(debug_assertions)]
#[tokio::test]
async fn a_patch_or_result_that_is_not_an_object_is_refused_in_debug_builds() {
    let Some((pool, p, _g)) = setup().await else {
        return;
    };
    let id = rq::enqueue(&pool, &boot_request(&p, json!({})))
        .await
        .unwrap();
    rq::claim_next(&pool, "test_boot").await.unwrap();
    let progress = tokio::spawn({
        let (pool, id) = (pool.clone(), id.clone());
        async move { rq::progress(&pool, &id, json!(["not", "an", "object"])).await }
    })
    .await;
    assert!(progress.unwrap_err().is_panic(), "progress took an array");
    let finish = tokio::spawn({
        let (pool, id) = (pool.clone(), id.clone());
        async move { rq::finish(&pool, &id, true, json!("done")).await }
    })
    .await;
    assert!(finish.unwrap_err().is_panic(), "finish took a string");
    assert_eq!(
        rq::get(&pool, &id).await.unwrap().unwrap().state,
        "running",
        "neither call reached the database"
    );
}
