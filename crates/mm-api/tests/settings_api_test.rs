//! HTTP contract of /_mm/admin/v1/settings* over a real listener + real Postgres.

use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use mm_api::middleware::{AuthConfig, apply_middleware};
use mm_api::settings_service::{BootOptions, SettingsService};
use mm_core::config::Config;
use mm_core::settings::crypto::KeyRing;
use mm_core::settings::{ApplyClass, ValueKind, registry};
use mm_db::test_support::require_or_try_pool;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";
const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const SECRET: &str = "mm-test-secret-7f3a";

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<PgPool> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("TRUNCATE mm_settings, mm_settings_audit").execute(&pool).await.unwrap();
    sqlx::query("UPDATE mm_settings_meta SET imported_at = NULL, restart_requested_rev = 0 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    Some(pool)
}

fn base() -> Config {
    let mut c = Config::default();
    c.server.cors_origins = vec!["https://a.example".into()];
    c.storage.s3.secret_key = SECRET.into();
    c
}

struct Api {
    url: String,
    svc: Arc<SettingsService>,
    restart: CancellationToken,
    http: reqwest::Client,
}

async fn start(pool: &PgPool, base: Config, keys: Option<KeyRing>) -> Api {
    let restart = CancellationToken::new();
    let svc = SettingsService::boot(pool.clone(), base, keys, BootOptions::for_tests(), restart.clone())
        .await
        .unwrap();
    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    let router = apply_middleware(
        axum::Router::new().nest("/_mm/admin/v1", mm_api::admin_settings::routes(svc.clone())),
        svc.handle().clone(),
        auth,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Api { url: format!("http://{addr}/_mm/admin/v1"), svc, restart, http: reqwest::Client::new() }
}

fn admin_jwt(role: &str) -> String {
    mm_core::auth::issue_admin_session_token("@op:example.org", role, JWT_KEY).unwrap()
}

impl Api {
    async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        let r = self.http.get(format!("{}{path}", self.url)).bearer_auth(token).send().await.unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    }
    async fn send(&self, method: reqwest::Method, path: &str, token: &str, body: Value, origin: Option<&str>) -> (StatusCode, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.url)).bearer_auth(token).json(&body);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let r = req.send().await.unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    }
    async fn patch(&self, changes: Value, extra: Value) -> (StatusCode, Value) {
        let rev = self.get("/settings", ADMIN_TOKEN).await.1["current_rev"].clone();
        let mut body = json!({"changes": changes, "expected_rev": rev});
        body.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
        self.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, body, None).await
    }
}

#[tokio::test]
async fn admins_see_values_but_never_secrets() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), KeyRing::from_values(Some(K1), None).unwrap()).await;
    let r = api.http.get(format!("{}/settings", api.url)).bearer_auth(ADMIN_TOKEN).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let text = r.text().await.unwrap();
    assert!(!text.contains(SECRET), "secret leaked into GET /settings");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["schema"].as_array().unwrap().len(), registry().len());
    assert_eq!(body["values"]["server.cors_origins"]["value"], json!(["https://a.example"]));
    assert_eq!(body["values"]["storage.s3.secret_key"]["is_set"], json!(true));
    assert!(body["values"]["storage.s3.secret_key"].get("value").is_none());
}

#[tokio::test]
async fn demo_sees_the_layout_with_values_hidden_and_cannot_write() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let demo = admin_jwt("demo");
    let (s, body) = api.get("/settings", &demo).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body["values"].as_object().unwrap().values().all(|v| v["value"] == "hidden"));
    assert_eq!(body["demo"], json!(true));
    let body = json!({"changes": {"recording.retention_days": 5}, "expected_rev": 0});
    let (s, e) = api.send(reqwest::Method::PATCH, "/settings", &demo, body, None).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::FORBIDDEN, Some("MM_FORBIDDEN")));
    assert_eq!(api.send(reqwest::Method::POST, "/settings/apply", &demo, json!({}), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.send(reqwest::Method::POST, "/settings/test/livekit", &demo, json!({}), None).await.0, StatusCode::FORBIDDEN);
    let (_, audit) = api.get("/settings/audit", &demo).await;
    assert!(audit.as_array().unwrap().iter().all(|r| r["actor"] == "hidden" && r["new_value"].is_null()));
}

#[tokio::test]
async fn a_live_cors_change_is_enforced_by_the_next_request() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let (s, body) = api.patch(json!({"server.cors_origins": ["https://z.example"]}), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let pre = api
        .http
        .request(reqwest::Method::OPTIONS, format!("{}/settings", api.url))
        .header("origin", "https://z.example")
        .header("access-control-request-method", "GET")
        .send()
        .await
        .unwrap();
    assert_eq!(pre.headers().get("access-control-allow-origin").unwrap(), "https://z.example");
}

#[tokio::test]
async fn a_stale_revision_is_409_with_the_current_state() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let body = json!({"changes": {"recording.retention_days": 5}, "expected_rev": -1});
    let (s, e) = api.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, body, None).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("MM_SETTINGS_CONFLICT")));
    assert_eq!(e["current"]["current_rev"], api.get("/settings", ADMIN_TOKEN).await.1["current_rev"]);
}

#[tokio::test]
async fn read_only_invalid_and_keyless_secret_changes_are_rejected() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let (s, e) = api.patch(json!({"jwt_signing_key": "x"}), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_SETTINGS_READ_ONLY")));
    assert!(e["message"].as_str().unwrap().contains("mmctl rotate"));
    let (_, e) = api.patch(json!({"matrix.hs_token": "x"}), json!({})).await;
    assert!(e["message"].as_str().unwrap().contains("Synapse"));
    let (s, e) = api.patch(json!({"turn.ttl_secs": 1}), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("MM_SETTINGS_INVALID")));
    assert_eq!(e["problems"][0]["key"], "turn.ttl_secs");
    let (s, e) = api.patch(json!({"storage.s3.access_key": SECRET}), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_SETTINGS_NO_KEY")));
    let (s, _) = api.patch(json!({}), json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_lockout_guard_protects_the_calling_browser() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let rev = api.get("/settings", ADMIN_TOKEN).await.1["current_rev"].clone();
    let body = json!({"changes": {"server.cors_origins": ["https://b.example"]}, "expected_rev": rev});
    let (s, e) = api.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, body.clone(), Some("https://a.example")).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("MM_SETTINGS_LOCKOUT")));
    let mut confirmed = body;
    confirmed["confirm_lockout"] = json!(true);
    let (s, _) = api.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, confirmed, Some("https://a.example")).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn secret_carrying_urls_are_read_only_and_moving_lnbits_needs_its_keys() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let mut b = base();
    b.monetization.lnbits_url = "http://lnbits:5000".into();
    b.monetization.lnbits_invoice_key = "inv-saved".into();
    let api = start(&pool, b, KeyRing::from_values(Some(K1), None).unwrap()).await;
    for key in [
        "matrix.homeserver_url", "matrix.public_homeserver_url", "monetization.stripe_api_base",
        "sfu.livekit_url", "sfu.livekit_public_url", "advertising.switch_url", "server.widget_dir",
        "server.public_url",
    ] {
        let (s, e) = api.patch(json!({ key: "https://elsewhere.example" }), json!({})).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_SETTINGS_READ_ONLY")), "{key}");
    }
    let (s, e) = api.patch(json!({"monetization.lnbits_url": "https://elsewhere.example"}), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("MM_SETTINGS_REENTER_SECRETS")));
    assert!(e["message"].as_str().unwrap().contains("monetization.lnbits_invoice_key"));
}

#[tokio::test]
async fn apply_restarts_when_behind_and_is_a_no_op_otherwise() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let (s, body) = api.send(reqwest::Method::POST, "/settings/apply", ADMIN_TOKEN, json!({}), None).await;
    assert_eq!((s, body["restarting_in_secs"].clone()), (StatusCode::OK, Value::Null));
    api.patch(json!({"server.drain_seconds": 45}), json!({})).await;
    let (s, body) = api.send(reqwest::Method::POST, "/settings/apply", ADMIN_TOKEN, json!({}), None).await;
    assert_eq!((s, body["restarting_in_secs"].clone()), (StatusCode::ACCEPTED, json!(1)));
    tokio::time::timeout(Duration::from_secs(2), api.restart.cancelled()).await.expect("restart");
}

#[tokio::test]
async fn apply_is_refused_with_409_while_a_stored_value_is_invalid() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    start(&pool, base(), None).await; // first boot imports
    sqlx::query("UPDATE mm_settings SET value_json = '\"not-a-list\"' WHERE key = 'server.cors_origins'")
        .execute(&pool)
        .await
        .unwrap();
    let api = start(&pool, base(), None).await; // boots in automatic safe mode
    let (s, e) = api.send(reqwest::Method::POST, "/settings/apply", ADMIN_TOKEN, json!({}), None).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("MM_SETTINGS_INVALID")));
    assert_eq!(e["problems"][0]["key"], "server.cors_origins");
    assert!(tokio::time::timeout(Duration::from_millis(200), api.restart.cancelled()).await.is_err());
}

#[tokio::test]
async fn audit_records_the_actor() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    api.patch(json!({"recording.retention_days": 5}), json!({})).await;
    let rev = api.get("/settings", ADMIN_TOKEN).await.1["current_rev"].clone();
    let body = json!({"changes": {"recording.retention_days": 6}, "expected_rev": rev});
    api.send(reqwest::Method::PATCH, "/settings", &admin_jwt("admin"), body, None).await;
    let (_, log) = api.get("/settings/audit?key=recording.retention_days&limit=2", ADMIN_TOKEN).await;
    assert_eq!(log[0]["actor"], "@op:example.org");
    assert_eq!(log[1]["actor"], "admin-token");
    assert_eq!((log[0]["old_value"].clone(), log[0]["new_value"].clone()), (json!(5), json!(6)));
}

#[tokio::test]
async fn test_connection_uses_the_form_values() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let ln = {
        let router = axum::Router::new().route(
            "/api/v1/wallet",
            axum::routing::get(|h: axum::http::HeaderMap| async move {
                if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("inv-form") {
                    axum::http::StatusCode::OK
                } else {
                    axum::http::StatusCode::FORBIDDEN
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    };
    let body = json!({"values": {"monetization.lnbits_url": ln, "monetization.lnbits_invoice_key": "inv-form"}});
    let (s, r) = api.send(reqwest::Method::POST, "/settings/test/lnbits", ADMIN_TOKEN, body, None).await;
    assert_eq!((s, r["ok"].clone()), (StatusCode::OK, json!(true)), "{r}");
}

/// A stub on 127.0.0.1 that answers 200 to anything and counts the requests it sees.
async fn counting_stub() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let router = axum::Router::new().fallback(move || {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"versions": ["v1.1"]}))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), hits)
}

/// A browser the running CORS list never allowed reached us same-origin (the dashboard
/// behind a proxy); a CORS change cannot lock it out, so it is not asked to confirm.
#[tokio::test]
async fn the_lockout_guard_ignores_an_origin_the_running_list_does_not_allow() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let rev = api.get("/settings", ADMIN_TOKEN).await.1["current_rev"].clone();
    let body = json!({"changes": {"server.cors_origins": ["https://b.example"]}, "expected_rev": rev});
    let (s, e) = api.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, body, Some("https://proxy.example")).await;
    assert_eq!(s, StatusCode::OK, "{e}");
    assert_eq!(api.svc.handle().load().server.cors_origins, vec!["https://b.example"]);
}

#[tokio::test]
async fn malformed_requests_are_400_json_that_never_echo_the_input() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    let bad = |body: Value| api.send(reqwest::Method::PATCH, "/settings", ADMIN_TOKEN, body, None);
    for body in [json!({"changes": SECRET, "expected_rev": 0}), json!({"changes": {"a": 1}, "expected_rev": SECRET})] {
        let (s, e) = bad(body).await;
        assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_INVALID_REQUEST")), "{e}");
        assert!(!e.to_string().contains(SECRET), "the error echoed the input: {e}");
    }
    let (s, e) = api.patch(json!({"no.such.setting": 1}), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_INVALID_REQUEST")));
    let (s, e) = api.send(reqwest::Method::POST, "/settings/test/ftp", ADMIN_TOKEN, json!({}), None).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_INVALID_REQUEST")));
    let (s, e) = api.get("/settings/audit?limit=many", ADMIN_TOKEN).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("MM_INVALID_REQUEST")));
    // Authorisation is decided before the body is looked at.
    let (s, _) = api.send(reqwest::Method::PATCH, "/settings", &admin_jwt("demo"), json!({"changes": 1}), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let r = api.http.get(format!("{}/settings", api.url)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_test_endpoint_never_sends_saved_secrets_to_a_form_chosen_host() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let mut b = base();
    b.monetization.lnbits_url = "http://lnbits:5000".into();
    b.monetization.lnbits_invoice_key = "inv-saved".into();
    let api = start(&pool, b, KeyRing::from_values(Some(K1), None).unwrap()).await;
    let (stub, hits) = counting_stub().await;

    let body = json!({"values": {"monetization.lnbits_url": stub}});
    let (s, r) = api.send(reqwest::Method::POST, "/settings/test/lnbits", ADMIN_TOKEN, body, None).await;
    assert_eq!((s, r["ok"].clone()), (StatusCode::OK, json!(false)), "{r}");
    assert!(r["detail"].as_str().unwrap().contains("monetization.lnbits_invoice_key"), "{r}");

    let body = json!({"values": {"matrix.homeserver_url": stub}});
    let (s, r) = api.send(reqwest::Method::POST, "/settings/test/homeserver", ADMIN_TOKEN, body, None).await;
    assert_eq!((s, r["ok"].clone()), (StatusCode::OK, json!(false)), "{r}");
    assert!(r["detail"].as_str().unwrap().contains("read-only"), "{r}");

    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "a refused test still reached the stub");
}

#[tokio::test]
async fn the_audit_limit_is_clamped_and_an_empty_key_filters_nothing() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), None).await;
    api.patch(json!({"recording.retention_days": 5}), json!({})).await;
    api.patch(json!({"recording.retention_days": 6}), json!({})).await;
    let (s, log) = api.get("/settings/audit?key=recording.retention_days&limit=0", ADMIN_TOKEN).await;
    assert_eq!((s, log.as_array().map(Vec::len)), (StatusCode::OK, Some(1)), "{log}");
    assert_eq!(log[0]["new_value"], json!(6), "newest first");
    let (s, log) = api.get("/settings/audit?key=&limit=100000", ADMIN_TOKEN).await;
    assert_eq!(s, StatusCode::OK, "{log}");
    assert!(log.as_array().unwrap().len() >= 2, "an empty key filters nothing: {log}");
}

/// A valid value different from `current`, for the registry-driven live test.
fn different(key: &str, kind: ValueKind, current: &Value) -> Value {
    match (key, kind) {
        ("server.cors_origins", _) => json!(["https://s.example"]),
        ("turn.urls", _) => json!(["turn:s.example:3478"]),
        (_, ValueKind::List) => json!(["s.example"]),
        (_, ValueKind::Bool) => json!(!current.as_bool().unwrap()),
        (_, ValueKind::Int { min, max }) => json!(if current.as_i64() == Some(min) { max } else { min }),
        (_, ValueKind::Float { min, max }) => json!(if current.as_f64() == Some(min) { max } else { min }),
        (_, ValueKind::Url | ValueKind::OptUrl) => json!("https://sample.example"),
        (_, ValueKind::Choice { options }) => json!(options.iter().find(|o| current.as_str() != Some(**o)).unwrap()),
        (_, ValueKind::Text | ValueKind::OptText) => json!("sample"),
    }
}

/// Spec §9: every Live setting changed through the API is in effect at once.
#[tokio::test]
async fn every_live_setting_takes_effect_without_a_restart() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let api = start(&pool, base(), KeyRing::from_values(Some(K1), None).unwrap()).await;
    for def in registry().iter().filter(|d| d.class == ApplyClass::Live) {
        let current = (def.get)(&api.svc.handle().load());
        let value = different(def.key, def.kind, &current);
        let (s, body) = api.patch(json!({ def.key: value }), json!({})).await;
        assert_eq!(s, StatusCode::OK, "{}: {body}", def.key);
        assert_eq!((def.get)(&api.svc.handle().load()), value, "{} not live", def.key);
    }
}

#[derive(Clone)]
struct Capture(Arc<StdMutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn secrets_never_reach_the_logs() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let sink = Capture(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);
    let api = start(&pool, base(), KeyRing::from_values(Some(K1), None).unwrap()).await;
    api.patch(json!({"storage.s3.access_key": SECRET}), json!({})).await;
    api.get("/settings", ADMIN_TOKEN).await;
    api.get("/settings/audit", ADMIN_TOKEN).await;
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(!logs.is_empty(), "the capture must actually see log lines");
    assert!(!logs.contains(SECRET), "secret found in logs");
}
