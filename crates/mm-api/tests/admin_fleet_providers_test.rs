//! HTTP contract of the GPU-provider admin API (`/_mm/admin/v1/broadcast-servers/providers…`)
//! against a real Postgres and the real router: validation, write-only credentials, demo
//! redaction, audit rows, request queueing.
//!
//! Every DB test holds one file-wide lock from before the migrations and table wipes until it
//! ends: the tests share the same tables and cargo runs them on parallel threads.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mm_api::admin_fleet_providers;
use mm_api::middleware::AuthConfig;
use mm_db::test_support::require_or_try_pool as try_pool;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

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
        .nest("/_mm/admin/v1", admin_fleet_providers::routes(pool))
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
    send(app, req).await
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
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

fn scaleway_input() -> Value {
    json!({"label": "Scaleway main", "kind": "scaleway", "enabled": true, "endpoint_display": "https://api.scaleway.com", "account_display": "proj-1",
           "image": "ubuntu_noble", "gpu_image": "ubuntu_noble_gpu_os_13_nvidia", "transcode_image": null, "max_gpu_nodes": 1,
           "zones": [{"zone": "fr-par-2", "region": "eu", "sizes": {"transcode": "L4-1-24G"}}]})
}

fn runpod_input() -> Value {
    let mut rp = scaleway_input();
    rp["kind"] = json!("runpod");
    rp["label"] = json!("RunPod");
    rp["endpoint_display"] = json!("https://rest.runpod.io/v1");
    rp["zones"] = json!([]);
    rp
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// The pool plus the file-wide lock guard: keep the guard bound for the whole test.
async fn fresh() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let guard = lock().lock().await;
    let pool = try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_fleet_nodes WHERE mm_node_id LIKE 'apitest-%'")
        .execute(&pool)
        .await
        .unwrap();
    for t in [
        "mm_fleet_requests",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_providers",
        "mm_fleet_control",
        "mm_fleet_ops_audit",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    Some((pool, guard))
}

async fn heartbeat(pool: &PgPool, key_id: &str, public_key: &[u8]) {
    mm_fleet::control_db::heartbeat(
        pool,
        &mm_fleet::control_db::Heartbeat {
            runner_version: "t",
            public_key,
            key_fingerprint: key_id,
            fleet_mode_seen: "frozen",
            settings_rev_seen: 0,
            detail: json!({"rented_nodes": 3}),
        },
    )
    .await
    .unwrap();
}

async fn create(app: &axum::Router, input: Value) -> String {
    let (s, v) = call(
        app,
        "POST",
        &format!("{BASE}/providers"),
        ADMIN_TOKEN,
        Some(input),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v["id"].as_str().unwrap().to_string()
}

async fn actions(pool: &PgPool, target: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM mm_fleet_ops_audit WHERE target = $1 ORDER BY id")
        .bind(target)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn create_list_update_delete_round_trip_with_audit() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/providers"),
        ADMIN_TOKEN,
        Some(scaleway_input()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let (s, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["demo"], false);
    assert_eq!(v["runner"]["reporting"], false);
    assert_eq!(v["providers"][0]["id"], id);
    assert_eq!(v["providers"][0]["billing_clock"], "minute");
    assert_eq!(v["providers"][0]["terraform_module"], "terraform/fleet");
    assert_eq!(
        v["providers"][0]["default_endpoint"],
        "https://api.scaleway.com"
    );
    assert_eq!(v["providers"][0]["account_display"], "proj-1");
    assert_eq!(v["providers"][0]["credential_set"], false);
    assert!(v["providers"][0]["credential"].is_null());
    let mut upd = scaleway_input();
    upd["label"] = json!("Scaleway EU");
    let (s, _) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{id}"),
        ADMIN_TOKEN,
        Some(upd),
    )
    .await;
    // Like every other write: 204, no body (the dashboard client only treats 204 as empty).
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["label"], "Scaleway EU");
    // Give it a token (through the handler) and a verdict: the Delete confirm says "Its token is deleted too".
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    let body = json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)});
    let (s, _) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{id}/credential"),
        ADMIN_TOKEN,
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    mm_fleet::providers_db::upsert_status(&pool, &status_row(&id))
        .await
        .unwrap();
    let stored = |table: &'static str| {
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM {table} WHERE provider_id = $1"
            ))
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(stored("mm_fleet_provider_credentials").await, 1);
    assert_eq!(stored("mm_fleet_provider_status").await, 1);
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("{BASE}/providers/{id}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(
        v["providers"].as_array().unwrap().len(),
        0,
        "a deleted provider is not listed"
    );
    assert_eq!(
        stored("mm_fleet_provider_credentials").await,
        0,
        "the sealed token is deleted with the provider"
    );
    assert_eq!(stored("mm_fleet_provider_status").await, 0);
    assert_eq!(
        actions(&pool, &id).await,
        vec![
            "provider_create",
            "provider_update",
            "credential_set",
            "provider_delete"
        ]
    );
}

#[tokio::test]
async fn validation_rejects_private_endpoints_and_bad_regions_without_echoing_values() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool);
    let uri = format!("{BASE}/providers");
    let mut bad = scaleway_input();
    bad["endpoint_display"] = json!("https://10.0.0.5/instance");
    let (s, v) = call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "MM_INVALID_REQUEST");
    assert!(!v["message"].as_str().unwrap().contains("10.0.0.5"));
    let mut bad = scaleway_input();
    bad["endpoint_display"] = json!("http://api.scaleway.com");
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = scaleway_input();
    bad["zones"][0]["region"] = json!("mars");
    let (s, v) = call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!v["message"].as_str().unwrap().contains("mars"));
    // A scaleway profile needs a zone; two zones may not share a name.
    let mut bad = scaleway_input();
    bad["zones"] = json!([]);
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = scaleway_input();
    bad["zones"] = json!([{"zone": "fr-par-2", "region": "eu", "sizes": {}}, {"zone": "fr-par-2", "region": "eu", "sizes": {}}]);
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = scaleway_input();
    bad["zones"][0]["sizes"] = json!({"gpu": "L4-1-24G"});
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = scaleway_input();
    bad["kind"] = json!("hetzner");
    let (s, v) = call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!v["message"].as_str().unwrap().contains("hetzner"));
    let mut bad = scaleway_input();
    bad["max_gpu_nodes"] = json!(101);
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = scaleway_input();
    bad["label"] = json!("x".repeat(81));
    assert_eq!(
        call(&app, "POST", &uri, ADMIN_TOKEN, Some(bad)).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_malformed_body_is_a_400_that_names_the_shape_not_the_input() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool);
    let req = Request::builder()
        .method("POST")
        .uri(format!("{BASE}/providers"))
        .header("authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"label": "hunter2-secret", "kind": 7"#))
        .unwrap();
    let (s, v) = send(&app, req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "MM_INVALID_REQUEST");
    assert!(!v.to_string().contains("hunter2"));
    // A field of the wrong type is the same.
    let mut wrong = scaleway_input();
    wrong["max_gpu_nodes"] = json!("hunter2-secret");
    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/providers"),
        ADMIN_TOKEN,
        Some(wrong),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!v.to_string().contains("hunter2"));
}

/// R20: the endpoint rule shares `mm_fleet::endpoint::ip_is_forbidden` with the runner and does
/// no DNS; the message names the rule and never the value.
#[test]
fn endpoint_rule_matches_the_runners_and_never_echoes_the_value() {
    for ok in [
        "https://api.scaleway.com",
        "https://rest.runpod.io/v1",
        "https://eu.api.ovh.com/1.0",
        "https://8.8.8.8/",
        "https://[2606:4700:4700::1111]/",
    ] {
        assert_eq!(admin_fleet_providers::validate_endpoint(ok), Ok(()), "{ok}");
    }
    for (bad, secret) in [
        ("not a url", "not a url"),
        ("http://api.scaleway.com", "api.scaleway.com"),
        ("https://user:pw@api.scaleway.com", "pw@"),
        ("https://10.0.0.5/x", "10.0.0.5"),
        ("https://127.0.0.1", "127.0.0.1"),
        ("https://169.254.169.254/latest/meta-data", "169.254"),
        ("https://100.64.0.1", "100.64"),
        ("https://192.168.1.1", "192.168"),
        ("https://0.0.0.0", "0.0.0.0"),
        ("https://[::1]/", "::1"),
        ("https://[fd00::1]/", "fd00"),
        ("https://[fe80::1]/", "fe80"),
        // Forms that are an IPv4 address in disguise.
        ("https://[::ffff:10.0.0.1]/", "ffff"),
        ("https://[64:ff9b::a00:1]/", "ff9b"),
        ("https://[2002:a00:1::]/", "2002"),
        // The URL parser normalises these to 127.0.0.1.
        ("https://2130706433/", "2130706433"),
        ("https://0x7f.1/", "0x7f"),
        ("https://localhost", "localhost"),
        ("https://LOCALHOST./", "LOCALHOST"),
        ("https://api.localhost", "api.localhost"),
        ("https://printer.local", "printer.local"),
        ("https://printer.local./", "printer.local"),
    ] {
        let err = admin_fleet_providers::validate_endpoint(bad).expect_err(bad);
        assert!(
            !err.to_lowercase().contains(&secret.to_lowercase()),
            "{bad} -> {err}"
        );
    }
}

#[tokio::test]
async fn credential_requires_the_current_runner_key_and_is_never_returned() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    let body = json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)});
    let uri = format!("{BASE}/providers/{id}/credential");
    // No runner row yet → the browser could not have sealed to a real key.
    let (s, v) = call(&app, "PUT", &uri, ADMIN_TOKEN, Some(body.clone())).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "MM_FLEET_RUNNER_KEY_CHANGED");
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    // A key the runner does not hold is the same answer.
    let mut other = body.clone();
    other["key_id"] = json!("0000000000000000");
    let (s, v) = call(&app, "PUT", &uri, ADMIN_TOKEN, Some(other)).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "MM_FLEET_RUNNER_KEY_CHANGED");
    let (s, _) = call(&app, "PUT", &uri, ADMIN_TOKEN, Some(body)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    let text = v.to_string();
    assert!(
        !text.contains(&"22".repeat(40)),
        "ciphertext must never be returned"
    );
    assert!(
        !text.contains(&"11".repeat(32)),
        "the encapsulated key must never be returned"
    );
    assert_eq!(
        v["providers"][0]["credential"]["key_id"],
        "ab12cd34ef567890"
    );
    assert_eq!(v["providers"][0]["credential"]["entered_by"], "admin-token");
    assert_eq!(v["providers"][0]["credential_set"], true);
    assert_eq!(v["runner"]["reporting"], true);
    assert_eq!(v["runner"]["public_key_hex"], "01".repeat(32));
    assert_eq!(v["runner"]["key_fingerprint"], "ab12cd34ef567890");
    assert_eq!(v["runner"]["version"], "t");
    assert_eq!(v["runner"]["fleet_mode_seen"], "frozen");
    assert_eq!(v["runner"]["rented_nodes"], 3);
    // The audit row names the key, never the bytes.
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM mm_fleet_ops_audit WHERE target = $1 AND action = 'credential_set'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(detail, json!({"key_id": "ab12cd34ef567890"}));
}

/// R23: a runner that is not reporting cannot open what is sealed to it, so entering a token
/// would only strand it; refuse with a different code than a rotated key.
#[tokio::test]
async fn credential_is_refused_while_the_runner_is_not_reporting() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    sqlx::query("UPDATE mm_fleet_control SET heartbeat_at = now() - interval '2 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let body = json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)});
    let (s, v) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{id}/credential"),
        ADMIN_TOKEN,
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "MM_FLEET_RUNNER_NOT_REPORTING");
    assert_eq!(
        v["message"],
        "the runner is not reporting; wait for its heartbeat and enter the token again"
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_provider_credentials")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "nothing was stored");
    // The list tells the page the same: the runner is not reporting and its key is withheld.
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(v["runner"]["reporting"], false);
    assert!(v["runner"]["heartbeat_at"].is_string());
    assert!(v["runner"]["public_key_hex"].is_null());
    assert!(v["runner"]["key_fingerprint"].is_null());
    assert!(v["runner"]["fleet_mode_seen"].is_null());
    assert!(v["runner"]["rented_nodes"].is_null());
}

#[tokio::test]
async fn credential_input_is_checked_and_can_be_cleared() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    let uri = format!("{BASE}/providers/{id}/credential");
    for (enc, ct) in [
        ("zz".repeat(32), "22".repeat(40)),
        ("11".repeat(31), "22".repeat(40)),
        ("11".repeat(32), "22".repeat(8)),
        ("11".repeat(32), "22".repeat(40) + "0"),
    ] {
        let body = json!({"key_id": "ab12cd34ef567890", "enc": enc, "ciphertext": ct});
        let (s, v) = call(&app, "PUT", &uri, ADMIN_TOKEN, Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(!v.to_string().contains(&"22".repeat(8)));
    }
    let (s, _) = call(&app, "PUT", &format!("{BASE}/providers/p-nope/credential"), ADMIN_TOKEN,
        Some(json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "PUT", &uri, ADMIN_TOKEN, Some(json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "DELETE", &uri, ADMIN_TOKEN, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, v) = call(&app, "DELETE", &uri, ADMIN_TOKEN, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["credential_set"], false);
    assert_eq!(
        actions(&pool, &id).await,
        vec!["provider_create", "credential_set", "credential_clear"]
    );
}

#[tokio::test]
async fn order_and_requests_and_bench() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let a = create(&app, scaleway_input()).await;
    let b = create(&app, runpod_input()).await;
    let order = format!("{BASE}/providers/order");
    let (s, v) = call(
        &app,
        "PUT",
        &order,
        ADMIN_TOKEN,
        Some(json!({"ids": [b, a]})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["id"], b);
    assert_eq!(v["providers"][0]["bench_state"], "pending");
    assert_eq!(v["providers"][0]["prepaid"], true);
    let order_detail: Value =
        sqlx::query_scalar("SELECT detail FROM mm_fleet_ops_audit WHERE action = 'provider_order'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(order_detail, json!({"before": [a, b], "after": [b, a]}));
    // The order must name every live provider exactly once.
    for bad in [
        json!({"ids": [a]}),
        json!({"ids": [a, a]}),
        json!({"ids": [a, b, "p-nope"]}),
    ] {
        assert_eq!(
            call(&app, "PUT", &order, ADMIN_TOKEN, Some(bad)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    // Bench: only a provider with a gate; the verdict is "passed" or "failed".
    let bench = format!("{BASE}/providers/{b}/bench");
    assert_eq!(
        call(
            &app,
            "POST",
            &bench,
            ADMIN_TOKEN,
            Some(json!({"result": "maybe"}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &bench,
            ADMIN_TOKEN,
            Some(json!({"result": "passed", "note": "n".repeat(501)}))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (s, _) = call(
        &app,
        "POST",
        &bench,
        ADMIN_TOKEN,
        Some(json!({"result": "passed", "note": "60 min, 0.2% loss"})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["bench_state"], "passed");
    assert_eq!(v["providers"][0]["bench_note"], "60 min, 0.2% loss");
    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/providers/{a}/bench"),
        ADMIN_TOKEN,
        Some(json!({"result": "passed"})),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "scaleway has no bench gate: {v}"
    );
    // A request needs a credential.
    let reqs = format!("{BASE}/providers/{a}/requests");
    let (s, v) = call(
        &app,
        "POST",
        &reqs,
        ADMIN_TOKEN,
        Some(json!({"kind": "test_connection"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    sqlx::query("INSERT INTO mm_fleet_provider_credentials (provider_id, key_id, enc, ciphertext, entered_by) VALUES ($1, 'k', '\\x00', '\\x00', 't')").bind(&a).execute(&pool).await.unwrap();
    let (s, v) = call(
        &app,
        "POST",
        &reqs,
        ADMIN_TOKEN,
        Some(json!({"kind": "test_connection", "reason": "after token swap"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let rid = v["id"].as_str().unwrap().to_string();
    let (s, v) = call(
        &app,
        "POST",
        &reqs,
        ADMIN_TOKEN,
        Some(json!({"kind": "test_connection"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "MM_FLEET_REQUEST_PENDING");
    let (s, v) = call(
        &app,
        "POST",
        &reqs,
        ADMIN_TOKEN,
        Some(json!({"kind": "test_boot", "zone": "fr-par-2"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = call(
        &app,
        "GET",
        &format!("{BASE}/requests/{rid}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "queued");
    assert_eq!(v["kind"], "test_connection");
    assert_eq!(v["provider_id"], a);
    assert_eq!(v["reason"], "after token swap");
    assert_eq!(
        actions(&pool, &a).await,
        vec!["provider_create", "request_create"]
    );
    assert_eq!(
        actions(&pool, &b).await,
        vec!["provider_create", "bench_record"]
    );
    let bench_row: (Option<String>, Value) = sqlx::query_as(
        "SELECT reason, detail FROM mm_fleet_ops_audit WHERE action = 'bench_record'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        bench_row,
        (
            Some("60 min, 0.2% loss".to_string()),
            json!({"result": "passed"})
        )
    );
}

#[tokio::test]
async fn unknown_ids_are_404_and_a_provider_cannot_change_kind() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    let nope = format!("{BASE}/providers/p-nope");
    assert_eq!(
        call(&app, "PUT", &nope, ADMIN_TOKEN, Some(scaleway_input()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", &nope, ADMIN_TOKEN, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("{nope}/bench"),
            ADMIN_TOKEN,
            Some(json!({"result": "passed"}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("{nope}/requests"),
            ADMIN_TOKEN,
            Some(json!({"kind": "test_connection"}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (s, v) = call(
        &app,
        "GET",
        &format!("{BASE}/requests/r-nope"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "MM_NOT_FOUND");
    let mut changed = scaleway_input();
    changed["kind"] = json!("runpod");
    changed["zones"] = json!([]);
    let (s, v) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{id}"),
        ADMIN_TOKEN,
        Some(changed),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // A deleted provider is gone for writes too.
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            Some(scaleway_input())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_provider_with_servers_cannot_be_deleted() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, provider_ref, destroy_deadline) VALUES ('apitest-n1', 'fanout', 'rented', 'scaleway', 'healthy', $1, now() + interval '1 hour')")
        .bind(&id).execute(&pool).await.unwrap();
    let (s, v) = call(
        &app,
        "DELETE",
        &format!("{BASE}/providers/{id}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "MM_FLEET_PROVIDER_IN_USE");
    assert_eq!(
        actions(&pool, &id).await,
        vec!["provider_create"],
        "a refused delete is not audited as a delete"
    );
    sqlx::query("DELETE FROM mm_fleet_nodes WHERE mm_node_id = 'apitest-n1'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &app,
            "DELETE",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
}

fn status_row(provider_id: &str) -> mm_fleet::providers_db::StatusRow {
    mm_fleet::providers_db::StatusRow {
        provider_id: provider_id.to_string(),
        checked_at: chrono::Utc::now(),
        state: "ok".into(),
        key_scope: Some("project".into()),
        quota: json!({"gpu": 2}),
        stock: json!({"fr-par-2": "available"}),
        prices: json!({"L4": 0.75}),
        balance_minor: Some(12_345),
        last_error: Some("provider said no".into()),
        last_error_kind: Some("quota".into()),
        last_error_at: Some(chrono::Utc::now()),
    }
}

#[tokio::test]
async fn status_is_shown_only_while_the_runner_is_reporting() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    mm_fleet::providers_db::upsert_status(&pool, &status_row(&id))
        .await
        .unwrap();
    let list = format!("{BASE}/providers");
    // No runner row: the stored status is a stale claim, not a fact.
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert!(v["providers"][0]["status"].is_null());
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["status"]["state"], "ok");
    assert_eq!(v["providers"][0]["status"]["balance_minor"], 12_345);
    sqlx::query("UPDATE mm_fleet_control SET heartbeat_at = now() - interval '2 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert!(v["providers"][0]["status"].is_null());
}

#[tokio::test]
async fn demo_sees_structure_only_and_cannot_write() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = create(&app, scaleway_input()).await;
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    let (s, _) = call(&app, "PUT", &format!("{BASE}/providers/{id}/credential"), ADMIN_TOKEN,
        Some(json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    mm_fleet::providers_db::upsert_status(&pool, &status_row(&id))
        .await
        .unwrap();

    let list = format!("{BASE}/providers");
    let demo = demo_token();
    let (s, v) = call(&app, "GET", &list, &demo, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["demo"], true);
    assert!(v["providers"][0]["account_display"].is_null());
    assert_eq!(v["providers"][0]["credential_set"], true);
    assert!(
        v["providers"][0]["credential"].is_null(),
        "no key id, no operator, no time"
    );
    assert_eq!(v["providers"][0]["zones"][0]["zone"], "fr-par-2");
    // The page still shows the verdict, but none of the provider-account values behind it.
    assert_eq!(v["providers"][0]["status"]["state"], "ok");
    let text = v.to_string();
    for leaked in [
        "proj-1",
        "admin-token",
        "12345",
        "provider said no",
        "available",
        "0.75",
    ] {
        assert!(!text.contains(leaked), "{leaked} leaked to demo: {text}");
    }
    assert!(v["providers"][0]["status"]["balance_minor"].is_null());
    assert!(v["providers"][0]["status"]["last_error"].is_null());
    // The whole redaction, field by field: the verdict, its time and the error KIND survive; every
    // provider-account value is blanked, not just absent from the text above.
    let st = &v["providers"][0]["status"];
    assert_eq!(st["provider_id"], id);
    assert!(st["checked_at"].is_string());
    assert_eq!(st["state"], "ok");
    assert_eq!(st["last_error_kind"], "quota");
    assert!(st["last_error_at"].is_string());
    assert!(st["key_scope"].is_null());
    assert_eq!(st["quota"], json!({}));
    assert_eq!(st["stock"], json!({}));
    assert_eq!(st["prices"], json!({}));

    let cred = json!({"key_id": "ab12cd34ef567890", "enc": "11".repeat(32), "ciphertext": "22".repeat(40)});
    for (m, p, b) in [
        ("POST", format!("{BASE}/providers"), Some(scaleway_input())),
        (
            "PUT",
            format!("{BASE}/providers/{id}"),
            Some(scaleway_input()),
        ),
        ("DELETE", format!("{BASE}/providers/{id}"), None),
        (
            "PUT",
            format!("{BASE}/providers/order"),
            Some(json!({"ids": [id]})),
        ),
        (
            "PUT",
            format!("{BASE}/providers/{id}/credential"),
            Some(cred),
        ),
        ("DELETE", format!("{BASE}/providers/{id}/credential"), None),
        (
            "POST",
            format!("{BASE}/providers/{id}/bench"),
            Some(json!({"result": "passed"})),
        ),
        (
            "POST",
            format!("{BASE}/providers/{id}/requests"),
            Some(json!({"kind": "test_connection"})),
        ),
        ("GET", format!("{BASE}/requests/r-anything"), None),
    ] {
        assert_eq!(
            call(&app, m, &p, &demo, b).await.0,
            StatusCode::UNAUTHORIZED,
            "{m} {p}"
        );
    }
    // Nothing the demo tried changed anything.
    assert_eq!(
        actions(&pool, &id).await,
        vec!["provider_create", "credential_set"]
    );
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert_eq!(v["providers"][0]["account_display"], "proj-1");
    assert_eq!(
        v["providers"][0]["credential"]["key_id"],
        "ab12cd34ef567890"
    );
}

#[tokio::test]
async fn no_token_and_a_wrong_token_are_401() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool);
    let req = Request::builder()
        .method("GET")
        .uri(format!("{BASE}/providers"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&app, "GET", &format!("{BASE}/providers"), "wrong", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
