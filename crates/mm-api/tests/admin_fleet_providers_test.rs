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

fn test_config(mode: mm_core::config::FleetMode) -> mm_core::config::Config {
    let mut c = mm_core::config::Config::default();
    c.server.public_url = Some("https://mm.example".into());
    c.fleet.mode = mode;
    c
}

fn app_with(pool: PgPool, config: mm_core::config::Config) -> axum::Router {
    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: "https://hs.example".into(),
    };
    axum::Router::new()
        .nest(
            "/_mm/admin/v1",
            admin_fleet_providers::routes(pool, mm_core::config_handle::ConfigHandle::new(config)),
        )
        .layer(axum::Extension(auth))
}

fn app(pool: PgPool) -> axum::Router {
    app_with(pool, test_config(mm_core::config::FleetMode::Frozen))
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
    sqlx::query("DELETE FROM mm_fleet_nodes WHERE mm_node_id LIKE 'apitest-%' OR mm_node_id LIKE 'tb-%' OR mm_node_id LIKE 'bc-apitest-%'")
        .execute(&pool)
        .await
        .unwrap();
    for t in [
        "mm_fleet_boot_tokens",
        "mm_fleet_zone_cooldown",
        "mm_fleet_desired",
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
        (
            "POST",
            format!("{BASE}/providers/{id}/requests"),
            Some(
                json!({"kind": "test_boot", "zone": "fr-par-2", "reason": "x", "confirmation": "test boot"}),
            ),
        ),
        (
            "POST",
            format!("{BASE}/nodes/tb-anything/drain"),
            Some(json!({"reason": "x"})),
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

// ---- GPU test boots, the Running GPU servers list, Release, and the in-use guards ----------

/// A Scaleway provider with a token and a fresh `ok` verdict newer than it, beside a runner
/// that is reporting.
async fn verified_provider(app: &axum::Router, pool: &PgPool) -> String {
    let id = create(app, scaleway_input()).await;
    heartbeat(pool, "ab12cd34ef567890", &[7u8; 32]).await;
    assert!(
        mm_fleet::providers_db::put_credential(
            pool,
            &id,
            &mm_fleet::providers_db::CredentialBlob {
                key_id: "ab12cd34ef567890".into(),
                enc: vec![1; 32],
                ciphertext: vec![2; 40],
                aad_version: 1,
            },
            "@argi:example",
        )
        .await
        .unwrap()
    );
    let mut s = status_row(&id);
    // Newer than the token, with room for a clock that differs between this host and Postgres.
    s.checked_at = chrono::Utc::now() + chrono::Duration::seconds(30);
    assert!(
        mm_fleet::providers_db::upsert_status(pool, &s)
            .await
            .unwrap()
    );
    id
}

fn test_boot_body() -> Value {
    json!({"kind": "test_boot", "zone": "fr-par-2", "reason": "prove fr-par-2", "confirmation": "test boot"})
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A live rented GPU node on the provider row `provider` (its id), in `zone`.
async fn live_node(pool: &PgPool, node: &str, provider: &str, zone: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, created_backend)
         VALUES ($1, 'transcode', 'rented', 'scaleway', 'healthy', now() + interval '10 minutes', $2, $3, 'api')",
    )
    .bind(node)
    .bind(provider)
    .bind(zone)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_test_boot_queues_a_request_and_a_pinned_desired_row() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/providers/{id}/requests"),
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v.as_object().unwrap().len(), 1, "only the id: {v}");
    let rid = v["id"].as_str().unwrap();
    let (s, r) = call(
        &app,
        "GET",
        &format!("{BASE}/requests/{rid}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["kind"], "test_boot");
    assert_eq!(r["state"], "queued");
    assert_eq!(r["zone"], "fr-par-2");
    assert_eq!(r["reason"], "prove fr-par-2");
    assert_eq!(
        r["params"]["report_url"],
        "https://mm.example/_mm/webhooks/fleet/boot-report"
    );
    assert!(
        r.as_object().unwrap().contains_key("result"),
        "the merged result is part of the view, null until the runner writes one"
    );
    let node = format!("tb-{}", rid.trim_start_matches("r-"));
    let desired: (String, Option<String>, Option<String>, String, String) = sqlx::query_as(
        "SELECT purpose, pinned_provider_id, pinned_zone, size, region FROM mm_fleet_desired WHERE mm_node_id = $1",
    )
    .bind(&node)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        desired,
        (
            "test_boot".to_string(),
            Some(id.clone()),
            Some("fr-par-2".to_string()),
            "L4-1-24G".to_string(),
            "eu".to_string()
        )
    );
    let acts = actions(&pool, &id).await;
    assert!(acts.contains(&"request_create".to_string()), "{acts:?}");
    let audit: Value = sqlx::query_scalar(
        "SELECT detail FROM mm_fleet_ops_audit WHERE action = 'request_create' AND target = $1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit["kind"], "test_boot");
    assert_eq!(audit["zone"], "fr-par-2");
    assert_eq!(
        count(&pool, "mm_fleet_boot_tokens").await,
        0,
        "mm-core mints no token: the runner does"
    );
}

#[tokio::test]
async fn a_test_boot_needs_the_exact_confirmation_and_a_zone_and_a_reason() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let url = format!("{BASE}/providers/{id}/requests");
    for confirmation in [
        json!("yes"),
        json!(""),
        json!("Test boot"),
        json!("TEST BOOT"),
        json!("test boot "),
        json!(" test boot"),
        json!("test  boot"),
        json!(null),
    ] {
        let mut b = test_boot_body();
        b["confirmation"] = confirmation.clone();
        let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(b)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{confirmation}: {v}");
    }
    let mut missing = test_boot_body();
    missing.as_object_mut().unwrap().remove("confirmation");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(missing)).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut bad_zone = test_boot_body();
    bad_zone["zone"] = json!("nl-ams-9");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(bad_zone))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut no_zone = test_boot_body();
    no_zone.as_object_mut().unwrap().remove("zone");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(no_zone)).await.0,
        StatusCode::BAD_REQUEST
    );
    for reason in [json!("  "), json!("r".repeat(501))] {
        let mut b = test_boot_body();
        b["reason"] = reason;
        assert_eq!(
            call(&app, "POST", &url, ADMIN_TOKEN, Some(b)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut no_reason = test_boot_body();
    no_reason.as_object_mut().unwrap().remove("reason");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(no_reason))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut unknown_kind = test_boot_body();
    unknown_kind["kind"] = json!("reboot");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(unknown_kind))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            "POST",
            &format!("{BASE}/providers/p-nope/requests"),
            ADMIN_TOKEN,
            Some(test_boot_body())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        count(&pool, "mm_fleet_requests").await,
        0,
        "nothing was queued"
    );
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);
    // The exact phrase, with a trimmed reason, is accepted.
    let mut ok = test_boot_body();
    ok["reason"] = json!("  proving  ");
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(ok)).await.0,
        StatusCode::ACCEPTED
    );
    let reason: String = sqlx::query_scalar("SELECT reason FROM mm_fleet_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reason, "proving");
}

#[tokio::test]
async fn a_test_boot_is_refused_with_a_reason_the_operator_can_act_on() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let url = format!("{BASE}/providers/{id}/requests");

    // Off blocks everything (D-C7).
    let off = app_with(pool.clone(), test_config(mm_core::config::FleetMode::Off));
    let (s, v) = call(&off, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_OFF"))
    );

    // No public URL: nowhere to report to.
    let mut no_url = test_config(mm_core::config::FleetMode::Frozen);
    no_url.server.public_url = None;
    let (s, _) = call(
        &app_with(pool.clone(), no_url),
        "POST",
        &url,
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);

    // One at a time.
    assert_eq!(
        call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body()))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_TEST_BOOT_RUNNING"))
    );

    // An unverified token.
    sqlx::query("UPDATE mm_fleet_provider_status SET state = 'needs_you'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mm_fleet_requests")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mm_fleet_desired")
        .execute(&pool)
        .await
        .unwrap();
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_NOT_VERIFIED"))
    );

    // An `ok` that is older than the freshness window, for a token entered before it.
    sqlx::query("UPDATE mm_fleet_provider_status SET state = 'ok', checked_at = now() - interval '16 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_provider_credentials SET entered_at = now() - interval '30 minutes'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_NOT_VERIFIED")),
        "a stale ok"
    );

    // A fresh `ok` reached before the current token was entered is about another token.
    sqlx::query("UPDATE mm_fleet_provider_status SET checked_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_provider_credentials SET entered_at = now() + interval '1 minute'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_NOT_VERIFIED")),
        "a verdict older than the token"
    );

    // A runner that is not reporting.
    sqlx::query("UPDATE mm_fleet_control SET heartbeat_at = now() - interval '5 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_RUNNER_NOT_REPORTING"))
    );

    // A provider with no token at all is a plain bad request.
    heartbeat(&pool, "ab12cd34ef567890", &[7u8; 32]).await;
    sqlx::query("DELETE FROM mm_fleet_provider_credentials")
        .execute(&pool)
        .await
        .unwrap();
    let (s, _) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);
}

#[tokio::test]
async fn a_zone_without_a_gpu_size_cannot_be_test_booted() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let mut input = scaleway_input();
    input["zones"] = json!([
        {"zone": "fr-par-2", "region": "eu", "sizes": {"transcode": "  "}},
        {"zone": "nl-ams-1", "region": "eu", "sizes": {"fanout": "DEV1-S"}},
    ]);
    let (s, _) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{id}"),
        ADMIN_TOKEN,
        Some(input),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let url = format!("{BASE}/providers/{id}/requests");
    for zone in ["fr-par-2", "nl-ams-1"] {
        let mut b = test_boot_body();
        b["zone"] = json!(zone);
        let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(b)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{zone}: {v}");
    }
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
}

#[tokio::test]
async fn a_public_url_the_probe_cannot_use_is_a_409_that_names_the_rule() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let id = verified_provider(&app(pool.clone()), &pool).await;
    let url = format!("{BASE}/providers/{id}/requests");
    for (public_url, rule) in [
        ("http://mm.example", "https"),
        ("https://deploy:hunter2@mm.example", "userinfo"),
        ("https://mm.example/?token=abc123", "query"),
        ("https://mm.example/#frag", "fragment"),
        ("not a url", "URL"),
    ] {
        let mut c = test_config(mm_core::config::FleetMode::Frozen);
        c.server.public_url = Some(public_url.into());
        let (s, v) = call(
            &app_with(pool.clone(), c),
            "POST",
            &url,
            ADMIN_TOKEN,
            Some(test_boot_body()),
        )
        .await;
        assert_eq!(
            (s, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("MM_FLEET_PUBLIC_URL_INVALID")),
            "{public_url}: {v}"
        );
        let text = v.to_string();
        assert!(text.contains(rule), "{public_url}: {text}");
        for leaked in ["hunter2", "abc123", "deploy"] {
            assert!(!text.contains(leaked), "{leaked} echoed: {text}");
        }
    }
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);

    // A path prefix is fine; the report path is appended to it.
    let mut c = test_config(mm_core::config::FleetMode::Frozen);
    c.server.public_url = Some("https://mm.example/media/".into());
    let app = app_with(pool.clone(), c);
    let (s, v) = call(&app, "POST", &url, ADMIN_TOKEN, Some(test_boot_body())).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let (_, r) = call(
        &app,
        "GET",
        &format!("{BASE}/requests/{}", v["id"].as_str().unwrap()),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(
        r["params"]["report_url"],
        "https://mm.example/media/_mm/webhooks/fleet/boot-report"
    );
}

#[tokio::test]
async fn the_test_boot_limits_each_have_their_own_answer() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let id = verified_provider(&app(pool.clone()), &pool).await;
    let url = format!("{BASE}/providers/{id}/requests");

    // The day's allowance: one boot, finished, uses it up.
    let mut c = test_config(mm_core::config::FleetMode::Frozen);
    c.fleet.test_boots_per_day = 1;
    c.fleet.max_gpu_nodes = 10;
    let one_a_day = app_with(pool.clone(), c);
    assert_eq!(
        call(
            &one_a_day,
            "POST",
            &url,
            ADMIN_TOKEN,
            Some(test_boot_body())
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    sqlx::query("UPDATE mm_fleet_requests SET state = 'done', finished_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mm_fleet_desired")
        .execute(&pool)
        .await
        .unwrap();
    let (s, v) = call(
        &one_a_day,
        "POST",
        &url,
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_TEST_BOOT_LIMIT")),
        "{v}"
    );
    let (_, q) = call(
        &one_a_day,
        "GET",
        &format!("{BASE}/gpu-nodes"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(
        q["test_boots"],
        json!({"per_day": 1, "used_today": 1, "left_today": 0})
    );

    // The fleet-wide GPU cap.
    sqlx::query("DELETE FROM mm_fleet_requests")
        .execute(&pool)
        .await
        .unwrap();
    let mut c = test_config(mm_core::config::FleetMode::Frozen);
    c.fleet.max_gpu_nodes = 0;
    let (s, v) = call(
        &app_with(pool.clone(), c),
        "POST",
        &url,
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_GPU_CAP")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("fleet"),
        "the fleet cap says so: {v}"
    );

    // This provider's own cap (one) with a machine already on it.
    live_node(&pool, "tb-cap", &id, "fr-par-2").await;
    let mut c = test_config(mm_core::config::FleetMode::Frozen);
    c.fleet.max_gpu_nodes = 10;
    let (s, v) = call(
        &app_with(pool.clone(), c),
        "POST",
        &url,
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_GPU_CAP")),
        "{v}"
    );
    assert!(
        v["message"].as_str().unwrap().contains("provider"),
        "the provider cap says so: {v}"
    );
    assert_eq!(count(&pool, "mm_fleet_requests").await, 0);
    assert_eq!(count(&pool, "mm_fleet_desired").await, 0);
}

#[tokio::test]
async fn gpu_servers_are_listed_with_cost_and_redacted_for_demo() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    // Billing began 4.5 minutes ago, so five minutes are billed: 5 min at 0.75/h is 0.0625,
    // which rounds up to the cent.
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref,
                                             provider_zone, size, purpose, created_by, created_backend, billing_started_at, provider_id)
                 VALUES ('tb-list', 'transcode', 'rented', 'scaleway', 'booting', now() + interval '10 minutes', $1,
                         'fr-par-2', 'L4', 'test_boot', '@argi:example', 'api', $2, 'fr-par-2/x')")
        .bind(&id)
        // This host's clock, not Postgres's: the handler bills against this host's clock, and the
        // two can differ by more than the 30 s this margin has.
        .bind(chrono::Utc::now() - chrono::Duration::seconds(270))
        .execute(&pool).await.unwrap();
    let (s, v) = call(&app, "GET", &format!("{BASE}/gpu-nodes"), ADMIN_TOKEN, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["nodes"].as_array().unwrap().len(), 1);
    let n = &v["nodes"][0];
    assert_eq!(
        (n["id"].as_str(), n["purpose"].as_str(), n["zone"].as_str()),
        (Some("tb-list"), Some("test_boot"), Some("fr-par-2"))
    );
    assert_eq!(n["provider_id"], id);
    assert_eq!(n["provider_label"], "Scaleway main");
    assert_eq!(n["kind"], "scaleway");
    assert_eq!(n["size"], "L4");
    assert_eq!(n["state"], "booting");
    assert!(n["broadcast_id"].is_null());
    assert_eq!(n["price_per_hour"], 0.75); // status_row's price for "L4"
    assert_eq!(n["est_cost"], 0.07); // 5 billed minutes at 0.75/h, rounded up
    assert_eq!(n["currency"], "EUR");
    assert_eq!(n["created_by"], "@argi:example");
    assert_eq!(n["request_id"], "r-list");
    assert!(n["billing_started_at"].is_string() && n["destroy_deadline"].is_string());
    assert!(n["boot_report"].is_null());
    assert_eq!(v["demo"], false);
    assert_eq!(
        v["test_boots"],
        json!({"per_day": 5, "used_today": 0, "left_today": 5})
    );
    assert_eq!(
        v["max_gpu_nodes"],
        mm_core::config::Config::default().fleet.max_gpu_nodes
    );
    assert_eq!(v["transcode_software_configured"], false);

    // Software counts only when an enabled provider names an image for it.
    let provider = format!("{BASE}/providers/{id}");
    for (image, enabled, configured) in [
        (json!("mm/transcoder:1"), true, true),
        (json!("mm/transcoder:1"), false, false),
        (json!("   "), true, false),
        (json!(null), true, false),
    ] {
        let mut input = scaleway_input();
        input["transcode_image"] = image.clone();
        input["enabled"] = json!(enabled);
        let (s, _) = call(&app, "PUT", &provider, ADMIN_TOKEN, Some(input)).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, v) = call(&app, "GET", &format!("{BASE}/gpu-nodes"), ADMIN_TOKEN, None).await;
        assert_eq!(
            v["transcode_software_configured"], configured,
            "{image} enabled={enabled}"
        );
    }

    let (s, d) = call(
        &app,
        "GET",
        &format!("{BASE}/gpu-nodes"),
        &demo_token(),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["demo"], true);
    let dn = &d["nodes"][0];
    assert!(dn["created_by"].is_null() && dn["boot_report"].is_null());
    assert!(
        dn["price_per_hour"].is_null() && dn["est_cost"].is_null(),
        "prices are provider-account values, redacted like the status prices"
    );
    // The structure still shows.
    assert_eq!(
        (dn["id"].as_str(), dn["state"].as_str(), dn["zone"].as_str()),
        (Some("tb-list"), Some("booting"), Some("fr-par-2"))
    );
    let text = d.to_string();
    for leaked in ["@argi:example", "0.75", "0.07"] {
        assert!(!text.contains(leaked), "{leaked} leaked to demo: {text}");
    }
}

#[tokio::test]
async fn a_broadcast_servers_broadcast_is_named_to_admins_only() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    // One whose desired row still says which broadcast, one whose desired row is gone (a
    // release ordered its teardown) so only its name does.
    for node in ["bc-apitest-a-transcode-0", "bc-apitest-b-transcode-1"] {
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, size, purpose, created_backend)
                     VALUES ($1, 'transcode', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $2, 'fr-par-2', 'L4', 'broadcast', 'api')")
            .bind(node).bind(&id).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline, purpose)
                 VALUES ('bc-apitest-a-transcode-0', 'transcode', 'rented', 'eu', 'L4', 'apitest-a', now() + interval '1 hour', 'broadcast')")
        .execute(&pool).await.unwrap();
    let (_, v) = call(&app, "GET", &format!("{BASE}/gpu-nodes"), ADMIN_TOKEN, None).await;
    let by_id = |v: &Value, id: &str| {
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        by_id(&v, "bc-apitest-a-transcode-0")["broadcast_id"],
        "apitest-a"
    );
    assert_eq!(
        by_id(&v, "bc-apitest-b-transcode-1")["broadcast_id"],
        "apitest-b",
        "named by its id once the desired row is gone"
    );
    assert!(by_id(&v, "bc-apitest-a-transcode-0")["request_id"].is_null());
    let (_, d) = call(
        &app,
        "GET",
        &format!("{BASE}/gpu-nodes"),
        &demo_token(),
        None,
    )
    .await;
    assert_eq!(d["nodes"].as_array().unwrap().len(), 2);
    for n in d["nodes"].as_array().unwrap() {
        assert!(n["broadcast_id"].is_null(), "{n}");
    }
}

#[tokio::test]
async fn a_boot_report_is_served_only_when_it_is_valid() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let valid = json!({"v": 1, "gpu": "NVIDIA L4, 550.54", "nvenc": "ok", "nvenc_error": null, "uptime_secs": 41, "probe_secs": 9});
    let reports = [
        (
            "tb-valid",
            json!({"report": valid, "received_at": "2026-10-08T10:00:00Z"}),
        ),
        (
            // An extra field a probe sent that the contract does not name.
            "tb-extra",
            json!({"report": {"v": 1, "gpu": "x", "nvenc": "ok", "uptime_secs": 1, "probe_secs": 1, "token": "SECRET-IN-REPORT"}, "received_at": "2026-10-08T10:00:00Z"}),
        ),
        (
            // Written without `validate`: ok with an error is a contradiction.
            "tb-contradiction",
            json!({"report": {"v": 1, "gpu": "x", "nvenc": "ok", "nvenc_error": "boom", "uptime_secs": 1, "probe_secs": 1}, "received_at": "2026-10-08T10:00:00Z"}),
        ),
        ("tb-shape", json!({"unexpected": "SECRET-IN-SHAPE"})),
        ("tb-string", json!("SECRET-IN-STRING")),
    ];
    for (node, report) in &reports {
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, size, purpose, created_backend, boot_report)
                     VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '10 minutes', $2, 'fr-par-2', 'L4', 'test_boot', 'api', $3)")
            .bind(node).bind(&id).bind(report).execute(&pool).await.unwrap();
    }
    let (_, v) = call(&app, "GET", &format!("{BASE}/gpu-nodes"), ADMIN_TOKEN, None).await;
    let report_of = |node: &str| {
        v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == node)
            .unwrap()["boot_report"]
            .clone()
    };
    assert_eq!(
        report_of("tb-valid"),
        json!({"report": valid, "received_at": "2026-10-08T10:00:00Z"})
    );
    for node in ["tb-extra", "tb-contradiction", "tb-shape", "tb-string"] {
        assert!(report_of(node).is_null(), "{node}: {}", report_of(node));
    }
    assert!(!v.to_string().contains("SECRET"), "{v}");
}

#[tokio::test]
async fn release_orders_the_teardown_and_records_who() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    let (_, v) = call(
        &app,
        "POST",
        &format!("{BASE}/providers/{id}/requests"),
        ADMIN_TOKEN,
        Some(test_boot_body()),
    )
    .await;
    let rid = v["id"].as_str().unwrap().to_string();
    let node = format!("tb-{}", rid.trim_start_matches("r-"));
    sqlx::query("UPDATE mm_fleet_requests SET state = 'running', claimed_at = now() WHERE id = $1")
        .bind(&rid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, purpose, created_backend, provider_id)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '10 minutes', $2, 'test_boot', 'api', 'fr-par-2/x')")
        .bind(&node).bind(&id).execute(&pool).await.unwrap();

    let url = format!("{BASE}/nodes/{node}/drain");
    for bad in [
        json!({"reason": ""}),
        json!({"reason": "   "}),
        json!({"reason": "r".repeat(501)}),
        json!({}),
    ] {
        assert_eq!(
            call(&app, "POST", &url, ADMIN_TOKEN, Some(bad)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &app,
            "POST",
            &url,
            &demo_token(),
            Some(json!({"reason": "x"}))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
            .bind(&node)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "booting", "nothing above released it");
    assert_eq!(
        call(
            &app,
            "POST",
            &url,
            ADMIN_TOKEN,
            Some(json!({"reason": "done looking"}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    let state: String =
        sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
            .bind(&node)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "destroying", "mm-core orders; the runner destroys");
    let desired: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE mm_node_id = $1")
            .bind(&node)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(desired, 0);
    let (_, r) = call(
        &app,
        "GET",
        &format!("{BASE}/requests/{rid}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(r["result"]["released_by"], "admin-token");
    assert_eq!(r["result"]["released_reason"], "done looking");
    assert_eq!(
        r["state"], "running",
        "the runner finishes the request, not the release"
    );
    let audit: (String, Option<String>, Value) = sqlx::query_as(
        "SELECT actor, reason, detail FROM mm_fleet_ops_audit WHERE action = 'release_rented' AND target = $1",
    )
    .bind(&node)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit.0, "admin-token");
    assert_eq!(audit.1.as_deref(), Some("done looking"));
    assert_eq!(audit.2["purpose"], "test_boot");
    let (s, v) = call(
        &app,
        "POST",
        &url,
        ADMIN_TOKEN,
        Some(json!({"reason": "again"})),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_ALREADY_RELEASED"))
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM mm_fleet_ops_audit WHERE action = 'release_rented'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1,
        "a refused second release is not audited"
    );
}

#[tokio::test]
async fn releasing_a_broadcast_transcoder_vetoes_it_for_that_broadcast() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    // Two broadcasts, each with a transcoder; only one is released.
    for b in ["apitest-bc1", "apitest-bc2"] {
        let room = format!("!{b}:hs");
        sqlx::query("DELETE FROM mm_streams WHERE id = $1")
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(&room)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mm_streams (id, room_id, host_user_id, status) SELECT $1, id, $2, 'active' FROM mm_rooms WHERE matrix_room_id = $3")
            .bind(b).bind(format!("@host-{b}:hs")).bind(&room).execute(&pool).await.unwrap();
        let node = format!("bc-{b}-transcode-0");
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, size, purpose, created_backend)
                     VALUES ($1, 'transcode', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $2, 'fr-par-2', 'L4', 'broadcast', 'api')")
            .bind(&node).bind(&id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline, purpose)
                     VALUES ($1, 'transcode', 'rented', 'eu', 'L4', $2, now() + interval '1 hour', 'broadcast')")
            .bind(&node).bind(b).execute(&pool).await.unwrap();
    }
    let released = |b: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT transcode_released FROM mm_streams WHERE id = $1")
                .bind(b)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(!released("apitest-bc1").await && !released("apitest-bc2").await);

    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/nodes/bc-apitest-bc1-transcode-0/drain"),
        ADMIN_TOKEN,
        Some(json!({"reason": "too costly"})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert!(released("apitest-bc1").await, "the broadcast is vetoed");
    assert!(!released("apitest-bc2").await, "no other broadcast is");
    let states: Vec<(String, String)> = sqlx::query_as("SELECT mm_node_id, state FROM mm_fleet_nodes WHERE mm_node_id LIKE 'bc-apitest-%' ORDER BY mm_node_id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        states,
        vec![
            (
                "bc-apitest-bc1-transcode-0".to_string(),
                "destroying".to_string()
            ),
            (
                "bc-apitest-bc2-transcode-0".to_string(),
                "healthy".to_string()
            ),
        ]
    );
    let audit: Value =
        sqlx::query_scalar("SELECT detail FROM mm_fleet_ops_audit WHERE action = 'release_rented'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit["broadcast_id"], "apitest-bc1");
    sqlx::query("DELETE FROM mm_streams WHERE id LIKE 'apitest-bc%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn only_rented_gpu_servers_can_be_released_here() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend)
                 VALUES ('tb-fanout', 'fanout', 'rented', 'scaleway', 'healthy', now() + interval '1 hour', $1, 'api')")
        .bind(&id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, created_backend)
                 VALUES ('tb-owned', 'transcode', 'owned', 'bare', 'healthy', 'terraform')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend)
                 VALUES ('tb-gone', 'transcode', 'rented', 'scaleway', 'gone', now() + interval '1 hour', $1, 'api')")
        .bind(&id).execute(&pool).await.unwrap();
    let body = Some(json!({"reason": "because"}));
    for node in ["tb-fanout", "tb-owned"] {
        let (s, v) = call(
            &app,
            "POST",
            &format!("{BASE}/nodes/{node}/drain"),
            ADMIN_TOKEN,
            body.clone(),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{node}: {v}");
    }
    let (s, v) = call(
        &app,
        "POST",
        &format!("{BASE}/nodes/tb-gone/drain"),
        ADMIN_TOKEN,
        body.clone(),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_ALREADY_RELEASED"))
    );
    let (s, _) = call(
        &app,
        "POST",
        &format!("{BASE}/nodes/tb-nope/drain"),
        ADMIN_TOKEN,
        body,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM mm_fleet_nodes ORDER BY mm_node_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        states,
        vec!["healthy", "gone", "healthy"],
        "fanout, gone, owned: untouched"
    );
    assert_eq!(
        count(&pool, "mm_fleet_ops_audit").await,
        1,
        "only provider_create"
    );
}

#[tokio::test]
async fn while_servers_run_the_provider_cannot_lose_what_destroys_them() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let id = verified_provider(&app, &pool).await;
    live_node(&pool, "tb-guard", &id, "fr-par-2").await;

    let credential = format!("{BASE}/providers/{id}/credential");
    let (s, v) = call(&app, "DELETE", &credential, ADMIN_TOKEN, None).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_IN_USE")),
        "clearing the token"
    );
    assert_eq!(
        count(&pool, "mm_fleet_provider_credentials").await,
        1,
        "the refused clear left the token"
    );
    for change in [
        json!({"endpoint_display": "https://api2.scaleway.example"}),
        json!({"account_display": "proj-2"}),
        json!({"account_display": null}),
        json!({"zones": [{"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G"}}]}),
        json!({"zones": [{"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G"}}, {"zone": "fr-par-1", "region": "eu", "sizes": {}}]}),
    ] {
        let mut input = scaleway_input();
        for (k, val) in change.as_object().unwrap() {
            input[k] = val.clone();
        }
        let (s, v) = call(
            &app,
            "PUT",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            Some(input),
        )
        .await;
        assert_eq!(
            (s, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_IN_USE")),
            "{change}"
        );
    }
    // None of the refusals changed the provider.
    let (_, list) = call(&app, "GET", &format!("{BASE}/providers"), ADMIN_TOKEN, None).await;
    let p = &list["providers"][0];
    assert_eq!(p["endpoint_display"], "https://api.scaleway.com");
    assert_eq!(p["account_display"], "proj-1");
    assert_eq!(p["zones"].as_array().unwrap().len(), 1);
    assert_eq!(p["zones"][0]["zone"], "fr-par-2");
    assert_eq!(p["credential_set"], true);

    // What does not touch how the machines are destroyed stays editable.
    let mut harmless = scaleway_input();
    harmless["label"] = json!("renamed");
    harmless["enabled"] = json!(false);
    harmless["max_gpu_nodes"] = json!(3);
    assert_eq!(
        call(
            &app,
            "PUT",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            Some(harmless)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let mut more_zones = scaleway_input();
    more_zones["zones"] = json!([
        {"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G"}},
        {"zone": "fr-par-2", "region": "eu", "sizes": {"transcode": "L4-1-24G", "fanout": "DEV1-S"}},
    ]);
    assert_eq!(
        call(
            &app,
            "PUT",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            Some(more_zones)
        )
        .await
        .0,
        StatusCode::NO_CONTENT,
        "a zone may be added, and reordered, while the live one stays"
    );
    // Replacing the token is allowed: the account binding keeps it on the same project.
    let (s, v) = call(
        &app,
        "PUT",
        &credential,
        ADMIN_TOKEN,
        Some(json!({"key_id": "ab12cd34ef567890", "enc": "33".repeat(32), "ciphertext": "44".repeat(40)})),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");

    // Once the machine is gone nothing depends on the provider any more.
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'tb-guard'")
        .execute(&pool)
        .await
        .unwrap();
    let mut moved = scaleway_input();
    moved["endpoint_display"] = json!("https://api2.scaleway.example");
    moved["account_display"] = json!("proj-2");
    moved["zones"] =
        json!([{"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G"}}]);
    assert_eq!(
        call(
            &app,
            "PUT",
            &format!("{BASE}/providers/{id}"),
            ADMIN_TOKEN,
            Some(moved)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "DELETE", &credential, ADMIN_TOKEN, None).await.0,
        StatusCode::NO_CONTENT
    );
}

/// The Q4 guards are decided under the provider row's lock. A machine's insert takes that row
/// (`FOR NO KEY UPDATE`) before it writes the node; an edit or a clear that read the live nodes
/// first and locked afterwards could miss a node committed in between. Here a machine's insert
/// is held open, the change is started behind it, and the change must see the machine.
#[tokio::test]
async fn the_guards_see_a_machine_committed_while_the_change_waited_for_the_provider_row() {
    let Some((pool, guard)) = fresh().await else {
        return;
    };
    let wide = sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    pool.close().await;
    let app = app(wide.clone());
    let id = verified_provider(&app, &wide).await;

    let mut endpoint = scaleway_input();
    endpoint["endpoint_display"] = json!("https://api2.scaleway.example");
    let mut zones = scaleway_input();
    zones["zones"] =
        json!([{"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G"}}]);
    let provider = format!("{BASE}/providers/{id}");
    let changes: Vec<(&str, &str, String, Option<Value>)> = vec![
        ("an endpoint edit", "PUT", provider.clone(), Some(endpoint)),
        ("a zone removal", "PUT", provider, Some(zones)),
        (
            "clearing the token",
            "DELETE",
            format!("{BASE}/providers/{id}/credential"),
            None,
        ),
    ];
    for (what, method, path, body) in changes {
        let node = format!("tb-race-{}", what.replace(' ', "-"));
        // A machine's insert in flight: it holds the provider row, as `insert_for_create` does.
        let mut inflight = wide.begin().await.unwrap();
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM mm_fleet_providers WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(&id)
        .fetch_one(&mut *inflight)
        .await
        .unwrap();
        let change = {
            let app = app.clone();
            tokio::spawn(async move { call(&app, method, &path, ADMIN_TOKEN, body).await })
        };
        // Prove the change is queued behind the lock (never with a sleep).
        wait_until_blocked(&wide, "FOR UPDATE", 1).await;
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, created_backend)
                     VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '10 minutes', $2, 'fr-par-2', 'api')")
            .bind(&node).bind(&id).execute(&mut *inflight).await.unwrap();
        inflight.commit().await.unwrap();
        let (s, v) = change.await.unwrap();
        assert_eq!(
            (s, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("MM_FLEET_PROVIDER_IN_USE")),
            "{what} did not see the machine committed ahead of it: {v}"
        );
        sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = $1")
            .bind(&node)
            .execute(&wide)
            .await
            .unwrap();
    }
    drop(guard);
}

/// Waits until at least `at_least` other backends are waiting on a lock while running a
/// statement that mentions `needle`: "blocked" is proven from `pg_stat_activity`, never a sleep.
async fn wait_until_blocked(pool: &PgPool, needle: &str, at_least: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND pid <> pg_backend_pid()
                AND datname = current_database()
                AND query LIKE '%' || $1 || '%'",
        )
        .bind(needle)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= at_least {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fewer than {at_least} backend(s) ever blocked on {needle}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_runner_view_names_its_new_fields_even_when_it_knows_nothing() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    create(&app, scaleway_input()).await;
    create(&app, runpod_input()).await;
    let runner = |v: &Value| v["runner"].as_object().unwrap().clone();
    let list = format!("{BASE}/providers");
    let fields = [
        "default_region",
        "create_backend_transcode",
        "create_backend_fanout",
    ];

    // No runner row: the keys are there and null (not omitted).
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    for f in fields {
        assert_eq!(runner(&v).get(f), Some(&Value::Null), "no runner: {f}");
    }
    heartbeat(&pool, "ab12cd34ef567890", &[1u8; 32]).await;
    // A fresh runner whose heartbeat carries no settings yet.
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert_eq!(v["runner"]["reporting"], true);
    for f in fields {
        assert_eq!(runner(&v).get(f), Some(&Value::Null), "no settings: {f}");
    }
    // A fresh runner that reports them.
    sqlx::query("UPDATE mm_fleet_control SET detail = $1")
        .bind(json!({"rented_nodes": 0, "settings": {"default_region": "eu", "create_backend_transcode": "api", "create_backend_fanout": "terraform", "other": "x"}}))
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert_eq!(v["runner"]["default_region"], "eu");
    assert_eq!(v["runner"]["create_backend_transcode"], "api");
    assert_eq!(v["runner"]["create_backend_fanout"], "terraform");
    // Not strings: absent, not coerced.
    sqlx::query("UPDATE mm_fleet_control SET detail = $1")
        .bind(json!({"settings": {"default_region": 7, "create_backend_transcode": null}}))
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    for f in fields {
        assert_eq!(runner(&v).get(f), Some(&Value::Null), "not strings: {f}");
    }
    // A runner that stopped reporting is withheld whole.
    sqlx::query(
        "UPDATE mm_fleet_control SET heartbeat_at = now() - interval '5 minutes', detail = $1",
    )
    .bind(json!({"settings": {"default_region": "eu"}}))
    .execute(&pool)
    .await
    .unwrap();
    let (_, v) = call(&app, "GET", &list, ADMIN_TOKEN, None).await;
    assert_eq!(v["runner"]["reporting"], false);
    for f in fields {
        assert_eq!(runner(&v).get(f), Some(&Value::Null), "stale: {f}");
    }
    // The currency each kind's prices are in.
    let currencies: Vec<(String, String)> = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["kind"].as_str().unwrap().into(),
                p["currency"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        currencies,
        vec![
            ("scaleway".to_string(), "EUR".to_string()),
            ("runpod".to_string(), "USD".to_string())
        ]
    );
}

/// The guarded edit runs the statements of `providers_db::update` inside its own transaction
/// (that function owns its transaction, so the guard cannot run in it). Until the guard moves
/// into `providers_db`, this holds the two together: every column and every zone and size a
/// profile has, written both ways from one input, reads back the same.
#[tokio::test]
async fn a_guarded_edit_writes_what_providers_db_writes() {
    let Some((pool, _guard)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let via_api = create(&app, scaleway_input()).await;
    let via_db = create(&app, scaleway_input()).await;
    let mut input = scaleway_input();
    input["label"] = json!("Changed");
    input["enabled"] = json!(false);
    input["endpoint_display"] = json!("https://api2.scaleway.example");
    input["account_display"] = json!("proj-9");
    input["image"] = json!("img-2");
    input["gpu_image"] = json!("gpu-2");
    input["transcode_image"] = json!("mm/transcoder:2");
    input["max_gpu_nodes"] = json!(4);
    input["zones"] = json!([
        {"zone": "nl-ams-1", "region": "eu", "sizes": {"transcode": "L4-1-24G", "fanout": "DEV1-S"}},
        {"zone": "fr-par-2", "region": "eu", "sizes": {}},
        {"zone": "pl-waw-1", "region": "eu", "sizes": {"edge": "DEV1-M"}},
    ]);
    let (s, v) = call(
        &app,
        "PUT",
        &format!("{BASE}/providers/{via_api}"),
        ADMIN_TOKEN,
        Some(input.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert!(
        mm_fleet::providers_db::update(&pool, &via_db, &serde_json::from_value(input).unwrap())
            .await
            .unwrap()
    );
    let read = |id: String| {
        let pool = pool.clone();
        async move {
            let p = mm_fleet::providers_db::get(&pool, &id)
                .await
                .unwrap()
                .unwrap();
            assert!(p.row.updated_at > p.row.created_at, "updated_at moved");
            let r = p.row;
            (
                (
                    r.label,
                    r.kind,
                    r.enabled,
                    r.endpoint_display,
                    r.account_display,
                ),
                (r.image, r.gpu_image, r.transcode_image, r.max_gpu_nodes),
                (r.bench_state, r.bench_note),
                p.zones,
            )
        }
    };
    assert_eq!(read(via_api).await, read(via_db).await);
}
