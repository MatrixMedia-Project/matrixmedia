//! HTTP contract of the manual relay lease admin API
//! (`/_mm/admin/v1/broadcast-servers/leases…`) against a real Postgres and the real router:
//! typed confirmation, idempotent replay, demo redaction, and release force semantics.
//!
//! Every DB test holds one file-wide lock from before the migrations and table wipes until it
//! ends: the tests share the same tables and cargo runs them on parallel threads.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mm_api::admin_fleet_leases;
use mm_api::middleware::AuthConfig;
use mm_db::test_support::require_or_try_pool as try_pool;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;
use uuid::Uuid;

const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";
const BASE: &str = "/_mm/admin/v1/broadcast-servers";

fn app(pool: PgPool) -> axum::Router {
    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: "https://hs.example".into(),
    };
    axum::Router::new()
        .nest("/_mm/admin/v1", admin_fleet_leases::routes(pool))
        .layer(axum::Extension(auth))
}

fn demo_token() -> String {
    mm_core::auth::issue_admin_session_token("@demo:example.org", "demo", JWT_KEY).unwrap()
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn fresh() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let guard = lock().lock().await;
    let pool = try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.unwrap();
    for t in ["mm_fleet_nodes", "mm_fleet_desired", "mm_usage_events"] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    Some((pool, guard))
}

fn rent_body(request_id: Uuid, count: u8) -> Value {
    json!({
        "request_id": request_id,
        "region": "eu-ams",
        "count": count,
        "confirmation": "rent relay",
    })
}

/// A rented fan-out node that already exists (not through the API's `create`, which the
/// runner alone turns into a machine) — enough for `list_leases` and `release_lease` to work
/// against, mirroring the shape `lease_db::create` itself writes to `mm_fleet_desired` plus
/// what the runner would have written to `mm_fleet_nodes` after a real rent.
async fn seed_leased_node(pool: &PgPool, id: &str, state: &str, viewers: i32) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired
             (mm_node_id, flavor, ownership, region, size, destroy_deadline, purpose,
              created_by, lease_source, lease_request_id, lease_duration_hint_secs)
         VALUES ($1, 'fanout', 'rented', 'eu-ams', 'planned', now() + interval '1 hour',
                 'broadcast', 'operator@example.com', 'manual', $2, 3600)",
    )
    .bind(id)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, state, viewers_current, viewer_capacity,
              destroy_deadline, billing_started_at)
         VALUES ($1, 'fanout', 'rented', 'hetzner', $2, $3, 1000, now() + interval '1 hour', now())",
    )
    .bind(id)
    .bind(state)
    .bind(viewers)
    .execute(pool)
    .await
    .unwrap();
}

// ---- POST (rent) --------------------------------------------------------------------------

#[tokio::test]
async fn create_lease_happy_path_writes_the_asked_count_of_desired_rows() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping create_lease_happy_path_writes_the_asked_count_of_desired_rows");
        return;
    };
    let app = app(pool.clone());
    let (status, v) = call(
        &app,
        "POST",
        &format!("{BASE}/leases"),
        ADMIN_TOKEN,
        Some(rent_body(Uuid::new_v4(), 3)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    let ids = v["node_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 3);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE lease_source = 'manual'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn replaying_the_same_request_id_returns_200_with_the_original_node_ids() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping replaying_the_same_request_id_returns_200_with_the_original_node_ids"
        );
        return;
    };
    let app = app(pool.clone());
    let request_id = Uuid::new_v4();
    let (first_status, first) = call(
        &app,
        "POST",
        &format!("{BASE}/leases"),
        ADMIN_TOKEN,
        Some(rent_body(request_id, 2)),
    )
    .await;
    assert_eq!(first_status, StatusCode::ACCEPTED, "{first}");

    let (second_status, second) = call(
        &app,
        "POST",
        &format!("{BASE}/leases"),
        ADMIN_TOKEN,
        Some(rent_body(request_id, 2)),
    )
    .await;
    assert_eq!(second_status, StatusCode::OK, "a replay must come back 200, not 202: {second}");
    assert_eq!(first["node_ids"], second["node_ids"], "a replay must return the same node ids");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE lease_source = 'manual'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "the replay must not have rented anything new");
}

#[tokio::test]
async fn missing_confirmation_is_refused_with_400_and_writes_nothing() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping missing_confirmation_is_refused_with_400_and_writes_nothing");
        return;
    };
    let app = app(pool.clone());
    let mut body = rent_body(Uuid::new_v4(), 1);
    body.as_object_mut().unwrap().remove("confirmation");
    let (status, v) = call(&app, "POST", &format!("{BASE}/leases"), ADMIN_TOKEN, Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn demo_role_is_refused_admin_actions() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping demo_role_is_refused_admin_actions");
        return;
    };
    let app = app(pool.clone());
    let (status, v) = call(
        &app,
        "POST",
        &format!("{BASE}/leases"),
        &demo_token(),
        Some(rent_body(Uuid::new_v4(), 1)),
    )
    .await;
    // `require_admin` refuses the demo role with `ErrorCode::Forbidden`, which this codebase
    // maps to 401 (see `mm-api/src/error.rs`), not 403 — matching
    // `require_admin_admits_admin_and_refuses_demo_with_the_forbidden_error` in middleware.rs.
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");
}

/// The composite partial unique index `(lease_request_id, mm_node_id)` — deferred from Task 6
/// — is what makes the idempotent replay safe against a race, not merely against sequential
/// calls: exercised here both through the API (the two replay calls above write to the SAME
/// rows without erroring) and directly, by trying to double-insert the exact pair underneath
/// the API and watching Postgres itself refuse it.
#[tokio::test]
async fn duplicate_lease_request_node_pair_is_rejected_by_the_composite_index() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping duplicate_lease_request_node_pair_is_rejected_by_the_composite_index"
        );
        return;
    };
    let request_id = Uuid::new_v4();
    let node_id = format!("lease-{request_id}-0");
    let insert = || {
        sqlx::query(
            "INSERT INTO mm_fleet_desired
                 (mm_node_id, flavor, ownership, region, size, destroy_deadline, purpose,
                  created_by, lease_source, lease_request_id)
             VALUES ($1, 'fanout', 'rented', 'eu-ams', 'planned', now() + interval '1 hour',
                     'broadcast', 'op', 'manual', $2)",
        )
        .bind(&node_id)
        .bind(request_id)
    };
    insert().execute(&pool).await.expect("first insert must succeed");
    let err = insert().execute(&pool).await.expect_err("a second insert of the same pair must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("mm_fleet_desired_lease_request_node_unique") || msg.contains("duplicate key"),
        "expected a unique-index violation, got: {msg}"
    );
}

// ---- GET (list) ---------------------------------------------------------------------------

#[tokio::test]
async fn list_leases_demo_role_sees_structure_only() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping list_leases_demo_role_sees_structure_only");
        return;
    };
    seed_leased_node(&pool, "lease-demo-test-0", "healthy", 5).await;
    let app = app(pool.clone());

    let (admin_status, admin_view) = call(&app, "GET", &format!("{BASE}/leases"), ADMIN_TOKEN, None).await;
    assert_eq!(admin_status, StatusCode::OK, "{admin_view}");
    assert_eq!(admin_view[0]["node_id"], json!("lease-demo-test-0"));

    let (demo_status, demo_view) = call(&app, "GET", &format!("{BASE}/leases"), &demo_token(), None).await;
    assert_eq!(demo_status, StatusCode::OK, "{demo_view}");
    assert_ne!(
        demo_view[0]["node_id"], json!("lease-demo-test-0"),
        "the demo role must not see the real node id"
    );
    assert_eq!(demo_view[0]["state"], json!("healthy"), "structure is still shown to demo");
}

// ---- DELETE (release) ---------------------------------------------------------------------

#[tokio::test]
async fn release_without_force_and_live_viewers_drains_not_evicts() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_without_force_and_live_viewers_drains_not_evicts");
        return;
    };
    seed_leased_node(&pool, "lease-release-test-0", "healthy", 7).await;
    let app = app(pool.clone());

    let (status, v) = call(
        &app,
        "DELETE",
        &format!("{BASE}/leases/lease-release-test-0?force=false"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["state"], json!("draining"), "force=false with viewers>0 must drain, not evict");

    let state: String = sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind("lease-release-test-0")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "draining");
}

#[tokio::test]
async fn release_with_force_evicts() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_with_force_evicts");
        return;
    };
    seed_leased_node(&pool, "lease-force-test-0", "healthy", 7).await;
    let app = app(pool.clone());

    let (status, v) = call(
        &app,
        "DELETE",
        &format!("{BASE}/leases/lease-force-test-0?force=true"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["state"], json!("evicted"));
}

#[tokio::test]
async fn release_of_an_unknown_node_is_404() {
    let Some((pool, _g)) = fresh().await else {
        eprintln!("MM_DATABASE_URL not set — skipping release_of_an_unknown_node_is_404");
        return;
    };
    let app = app(pool.clone());
    let (status, v) = call(
        &app,
        "DELETE",
        &format!("{BASE}/leases/no-such-node"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
}
