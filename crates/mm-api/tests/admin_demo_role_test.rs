//! Role contract of the admin API, on the real admin router (`mm_api::admin_router`).
//!
//! The read-only demo role is an ALLOWLIST: it may call only what its dashboard pages
//! fetch plus a few deliberate demo endpoints; every other route answers it with the
//! `MM_FORBIDDEN` / "admin access required" error, checked before any feature or database
//! guard (so the answer is the same whether or not monetization / advertising are on).
//! The three ad read routes used to take no token at all.
//!
//! The router needs a full `AppState`, which in turn needs a real Postgres (the settings
//! service boots against it): the tests skip without `MM_DATABASE_URL`, and fail loudly
//! with `MM_REQUIRE_DB` set, like every other DB-backed test here. The state has no pg
//! pool for monetization and no ad engine, so an *admitted* caller meets a feature guard
//! (501) on those routes — that is how the tests tell "admitted" from "refused".

use std::collections::HashMap;
use std::sync::Arc;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use mm_api::middleware::AuthConfig;
use mm_api::settings_service::{BootOptions, SettingsService};
use mm_api::state::{AppState, SharedState};
use mm_core::cache::TokenCache;
use mm_core::config::Config;
use mm_core::metrics::Metrics;
use mm_db::PgDatabase;
use mm_db::test_support::require_or_try_pool;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_sfu::CircuitBreakerAdapter;
use mm_sfu::livekit::LiveKitAdapter;

const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";
/// Nothing listens on the discard port: every outbound call the allowed routes make
/// (homeserver, LiveKit) is refused at once instead of reaching a real service.
const DEAD: &str = "http://127.0.0.1:9";

/// What `ErrorCode::Forbidden` answers on the wire (`mm-api/src/error.rs`), for the demo
/// refusal and for a missing token alike; the two differ in the message.
const REFUSED: StatusCode = StatusCode::UNAUTHORIZED;

async fn start() -> Option<String> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    let database_url = std::env::var("MM_DATABASE_URL").expect("require_or_try_pool saw it");
    let db = PgDatabase::new(&database_url).await.expect("database");

    let mut config = Config::default();
    config.matrix.homeserver_url = DEAD.into();
    config.sfu.livekit_url = Some(DEAD.into());
    let settings = SettingsService::boot(
        db.pool().clone(),
        config.clone(),
        None,
        BootOptions::for_tests(),
        CancellationToken::new(),
    )
    .await
    .expect("settings boot");

    let hs_client = HomeserverClient::new(DEAD.into(), String::new(), "@bot:example.org".into());
    let signup_pool = db.pool().clone();
    let state: SharedState = Arc::new(AppState {
        sfu: Box::new(CircuitBreakerAdapter::new(LiveKitAdapter::new(
            DEAD.into(),
            "key".into(),
            "secret".into(),
            None,
        ))),
        appservice_handler: AppserviceHandler::new(hs_client.clone()),
        hs_client,
        db: Box::new(db),
        token_cache: TokenCache::default(),
        config_handle: settings.handle().clone(),
        settings,
        metrics: Metrics::new(),
        started_at: std::time::Instant::now(),
        pg_pool: None,
        stripe_client: None,
        payment_registry: None,
        lnurl_client: mm_payment::lnurl::LnurlPayClient::new(),
        entitlement_service: None,
        redis: None,
        ad_engine: None,
        switch_client: None,
        broadcast_servers: Arc::new(mm_api::broadcast_servers::SnapshotCell::new()),
        ad_switches: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        signup_limiter: mm_api::rate_limit::LiveQuotaLimiter::new(5),
        signup_avail_limiter: mm_api::rate_limit::SignupRateLimiter::new(60),
        synapse_admin: Arc::new(mm_core::synapse_admin::SynapseAdminClient::new(
            DEAD,
            String::new(),
        )),
        signup_pool,
        announcement_cache: Arc::new(moka::future::Cache::builder().max_capacity(1).build()),
        feed_cache: Arc::new(moka::future::Cache::builder().max_capacity(10).build()),
        feed_limiter: mm_api::rate_limit::SignupRateLimiter::new(1800),
        moderation_report_limiter: mm_api::rate_limit::SignupRateLimiter::new(10),
        permissions_cache: Arc::new(moka::future::Cache::builder().max_capacity(10).build()),
        trending_engine: None,
    });

    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    let router = mm_api::admin_router(state).layer(axum::Extension(auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Some(format!("http://{addr}/_mm/admin/v1"))
}

fn jwt(role: &str) -> String {
    mm_core::auth::issue_admin_session_token("@op:example.org", role, JWT_KEY).unwrap()
}

#[derive(Clone, Copy)]
enum Payload {
    None,
    Json(&'static str),
    /// A multipart body with no parts — enough to get past the extractor.
    Multipart,
}

#[derive(Clone, Copy)]
struct Route {
    method: &'static str,
    path: &'static str,
    payload: Payload,
}

const fn get(path: &'static str) -> Route {
    Route {
        method: "GET",
        path,
        payload: Payload::None,
    }
}
const fn send(method: &'static str, path: &'static str, payload: Payload) -> Route {
    Route {
        method,
        path,
        payload,
    }
}

async fn call(base: &str, route: Route, token: Option<&str>) -> (StatusCode, Value) {
    let method = Method::from_bytes(route.method.as_bytes()).unwrap();
    let mut req = reqwest::Client::new().request(method, format!("{base}{}", route.path));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req = match route.payload {
        Payload::None => req,
        Payload::Json(body) => req
            .header("content-type", "application/json")
            .body(body.to_string()),
        Payload::Multipart => req
            .header("content-type", "multipart/form-data; boundary=x")
            .body("--x--\r\n"),
    };
    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), req.send())
        .await
        .unwrap_or_else(|_| panic!("{} {} timed out", route.method, route.path))
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// A response body for a failure message: the first 160 characters, never a whole data set.
fn short(body: &Value) -> String {
    body.to_string().chars().take(160).collect()
}

fn is_demo_refusal(status: StatusCode, body: &Value) -> bool {
    status == REFUSED
        && body["error"] == "MM_FORBIDDEN"
        && body["message"] == "admin access required"
}

/// Reads the demo role must not make: they return real data. Paths are valid, so a
/// refusal can only come from the handler's own check.
fn denied_reads() -> Vec<Route> {
    vec![
        // Reads the dashboard's demo pages never fetch (ruling F15).
        get("/streams"),
        get("/recordings"),
        get("/donations"),
        get("/lightning-stats"),
        get("/subscriptions"),
        get("/content-gates"),
        get("/creators"),
        get("/platform/metrics-summary"),
        get("/platform/revenue"),
        get("/platform/federation"),
        get("/platform/deployment"),
        get("/ads"),
        get("/ads/some-ad/stats"),
        get("/ads/analytics"),
        get("/announcements"),
        // Reads that were already refused.
        get("/synapse/users"),
        get("/server-requests"),
        get("/moderation/reports"),
        get("/moderation/reports/5b1e7c2d-0000-4000-8000-0000000000aa"),
        get("/moderation/audit?target_type=stream&target_id=x"),
        get("/moderation/users/@someone:example.org"),
    ]
}

/// Writes, all already refused: pinned so the allowlist cannot loosen them. Bodies are
/// valid, so the refusal comes from the handler, not from a failed extractor.
fn denied_writes() -> Vec<Route> {
    vec![
        send("DELETE", "/streams/some-stream", Payload::None),
        send("DELETE", "/recordings/some-recording", Payload::None),
        send("POST", "/recordings/cleanup", Payload::None),
        send(
            "PUT",
            "/donations/some-donation/status",
            Payload::Json(r#"{"status":"refunded"}"#),
        ),
        send("DELETE", "/content-gates/some-gate", Payload::None),
        send(
            "PUT",
            "/creators/@someone:example.org/onboarding",
            Payload::Json(r#"{"onboarding_complete":true}"#),
        ),
        send(
            "PUT",
            "/synapse/users/@someone:example.org",
            Payload::Json("{}"),
        ),
        send(
            "POST",
            "/synapse/deactivate/@someone:example.org",
            Payload::None,
        ),
        send("POST", "/ads", Payload::Json("{}")),
        send("PUT", "/ads/some-ad", Payload::Json("{}")),
        send("DELETE", "/ads/some-ad", Payload::None),
        send("POST", "/ads/some-ad/upload", Payload::Multipart),
        send(
            "POST",
            "/announcements",
            Payload::Json(r#"{"severity":"info","body":"x","expires_at":"2999-01-01T00:00:00Z"}"#),
        ),
        send("DELETE", "/announcements/1", Payload::None),
        send(
            "PUT",
            "/server-requests/some-request/status",
            Payload::Json(r#"{"status":"approved"}"#),
        ),
        send(
            "PUT",
            "/moderation/reports/5b1e7c2d-0000-4000-8000-0000000000aa/status",
            Payload::Json(r#"{"status":"open"}"#),
        ),
        send("POST", "/moderation/reports/sync", Payload::None),
        send(
            "POST",
            "/moderation/actions",
            Payload::Json(r#"{"action_type":"takedown","target_type":"stream","target_id":"x"}"#),
        ),
    ]
}

/// Dashboard-settings writes (`admin_settings::routes`): admin-only inside the settings
/// module, so they are not in the demo tables, but they must refuse a request with no
/// token. `expected_rev` can never match, so even a request that got through would write
/// nothing.
fn settings_writes() -> Vec<Route> {
    vec![
        send(
            "PATCH",
            "/settings",
            Payload::Json(
                r#"{"changes":{"server.cors_origins":["https://a.example"]},"expected_rev":-1}"#,
            ),
        ),
        send("POST", "/settings/apply", Payload::None),
        send("POST", "/settings/test/s3", Payload::None),
    ]
}

fn denied_for_demo() -> Vec<Route> {
    let mut all = denied_reads();
    all.extend(denied_writes());
    all
}

/// What the demo role may call: the dashboard's three demo pages and the deliberate demo
/// endpoints, with the status each answers in this setup.
fn allowed_for_demo() -> Vec<(Route, StatusCode)> {
    vec![
        (get("/health"), StatusCode::OK),
        (get("/stats"), StatusCode::OK),
        (get("/system-health"), StatusCode::OK),
        (get("/auth-info"), StatusCode::OK),
        (get("/platform/config-full"), StatusCode::OK),
        (get("/settings"), StatusCode::OK),
        (get("/settings/audit"), StatusCode::OK),
        (get("/broadcast-servers"), StatusCode::OK),
        // An empty org name passes the extractor and fails the handler's own validation,
        // before anything is written.
        (
            send(
                "POST",
                "/server-requests",
                Payload::Json(
                    r#"{"org_name":"","contact_email":"a@b.c","region":"eu","instance_size":"s"}"#,
                ),
            ),
            StatusCode::BAD_REQUEST,
        ),
        // Login needs no token; the dead homeserver refuses, which is not a role refusal.
        (
            send(
                "POST",
                "/login",
                Payload::Json(r#"{"user_id":"@someone:example.org","password":"pw"}"#),
            ),
            REFUSED,
        ),
    ]
}

#[tokio::test]
async fn demo_is_refused_every_route_that_returns_real_data_or_acts() {
    let Some(base) = start().await else { return };
    let demo = jwt("demo");
    let mut wrong = vec![];
    for route in denied_for_demo() {
        let (status, body) = call(&base, route, Some(&demo)).await;
        if !is_demo_refusal(status, &body) {
            wrong.push(format!(
                "{} {} -> {status} {}",
                route.method,
                route.path,
                short(&body)
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "demo was not refused on:\n{}",
        wrong.join("\n")
    );
}

#[tokio::test]
async fn demo_still_reaches_what_its_pages_and_the_demo_endpoints_use() {
    let Some(base) = start().await else { return };
    let demo = jwt("demo");
    for (route, expected) in allowed_for_demo() {
        let (status, body) = call(&base, route, Some(&demo)).await;
        assert!(
            !is_demo_refusal(status, &body),
            "demo on {} {} must be admitted, got {status} {}",
            route.method,
            route.path,
            short(&body)
        );
        assert_eq!(
            status,
            expected,
            "{} {}: {}",
            route.method,
            route.path,
            short(&body)
        );
    }
    // The config stub is the whole point of that route for demo.
    let (_, body) = call(&base, get("/platform/config-full"), Some(&demo)).await;
    assert_eq!(body, json!({ "demo": true }));
}

#[tokio::test]
async fn an_admin_is_admitted_to_every_read_the_demo_role_is_refused() {
    let Some(base) = start().await else { return };
    // The static token and an admin session alike. What an admitted caller then meets
    // (a feature guard, a missing row, a failed upstream) is none of this test's business;
    // it must only be neither the demo refusal nor any refusal at all (`REFUSED` is also
    // what a rejected token answers, so an admin credential that stops working shows up
    // here). Reads only: an admin's writes would act on the database.
    let admins = [ADMIN_TOKEN.to_string(), jwt("admin")];
    for token in &admins {
        for route in denied_reads() {
            let (status, body) = call(&base, route, Some(token)).await;
            assert!(
                !is_demo_refusal(status, &body) && status != REFUSED,
                "admin on {} {} was refused: {status} {}",
                route.method,
                route.path,
                short(&body)
            );
        }
    }
    // Pin the three ad reads: no ad engine / pool here, so admitted means a feature guard.
    for token in &admins {
        for path in ["/ads", "/ads/some-ad/stats", "/ads/analytics"] {
            let (status, body) = call(&base, get(path), Some(token)).await;
            assert_eq!(
                status,
                StatusCode::NOT_IMPLEMENTED,
                "GET {path}: {}",
                short(&body)
            );
        }
    }
}

#[tokio::test]
async fn the_ad_reads_need_a_token_and_so_does_everything_but_login_and_auth_info() {
    let Some(base) = start().await else { return };

    // The finding: these three answered with NO token.
    let mut wrong = vec![];
    for path in ["/ads", "/ads/some-ad/stats", "/ads/analytics"] {
        let (status, body) = call(&base, get(path), None).await;
        if status != REFUSED || body["error"] != "MM_FORBIDDEN" || is_demo_refusal(status, &body) {
            wrong.push(format!(
                "GET {path} without a token -> {status} {}",
                short(&body)
            ));
        }
        let (status, body) = call(
            &base,
            get(path),
            Some("not-a-token-0123456789abcdef0123456789abcdef"),
        )
        .await;
        if status != REFUSED {
            wrong.push(format!(
                "GET {path} with a wrong token -> {status} {}",
                short(&body)
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the ad reads are open:\n{}",
        wrong.join("\n")
    );

    // And nothing else is open: every other route refuses a request with no token.
    let open = ["/auth-info", "/login"];
    let routes = denied_for_demo()
        .into_iter()
        .chain(allowed_for_demo().into_iter().map(|(r, _)| r))
        .chain(settings_writes())
        .filter(|r| !open.contains(&r.path));
    let mut wrong = vec![];
    for route in routes {
        let (status, body) = call(&base, route, None).await;
        if status != REFUSED {
            wrong.push(format!(
                "{} {} -> {status} {}",
                route.method,
                route.path,
                short(&body)
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "no token was not refused on:\n{}",
        wrong.join("\n")
    );
}
