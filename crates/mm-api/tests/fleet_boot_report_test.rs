//! The public endpoint a test machine reports to. Anyone on the internet can call it, so each
//! refusal is checked: no token, a wrong/used/expired token, a bad body, too many calls.
//!
//! Every DB test holds one file-wide lock from before the migrations and table wipes until it
//! ends: the tests share the same tables and cargo runs them on parallel threads.

use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use mm_api::fleet_boot_report;
use mm_api::middleware::AuthConfig;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::test_boot;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn fresh() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let guard = lock().lock().await;
    let pool = try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_fleet_boot_tokens")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mm_fleet_nodes WHERE mm_node_id LIKE 'tb-report-%'")
        .execute(&pool)
        .await
        .unwrap();
    Some((pool, guard))
}

fn app(pool: PgPool) -> axum::Router {
    axum::Router::new().nest("/_mm/webhooks", fleet_boot_report::routes(pool))
}

/// The router wrapped as the client listener wraps it (the global 1 MiB body limit, request
/// tracing, CORS, request ids), so a test sees what the internet sees.
fn app_in_production_stack(pool: PgPool) -> axum::Router {
    let auth = AuthConfig {
        jwt_signing_key: String::new(),
        admin_token: String::new(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    mm_api::middleware::apply_middleware(
        app(pool),
        mm_core::config_handle::ConfigHandle::new(mm_core::config::Config::default()),
        auth,
    )
}

/// The database's own clock. The test container's clock can drift from the host's, and a
/// token's expiry is judged by the database.
async fn db_now(pool: &PgPool) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn node_with_token(pool: &PgPool, id: &str, ttl: Duration) -> String {
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, purpose, created_backend)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', now() + interval '15 minutes', 'test_boot', 'api')")
        .bind(id).execute(pool).await.unwrap();
    let (token, hash) = test_boot::mint_token();
    mm_fleet::test_boot_db::store_token(
        pool,
        &mm_core::fleet::NodeId::new(id),
        &hash,
        db_now(pool).await + ttl,
    )
    .await
    .unwrap();
    token
}

fn report() -> Value {
    json!({"v": 1, "gpu": "NVIDIA L4, 550.90.07", "nvenc": "ok", "nvenc_error": null, "uptime_secs": 95, "probe_secs": 71})
}

/// Everything an answer carries, so two answers can be compared byte for byte.
#[derive(Debug, PartialEq)]
struct Answer {
    status: StatusCode,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

/// `authorization` is the whole header value (or none), so a wrong scheme can be sent too.
async fn send(app: &axum::Router, authorization: Option<String>, body: String, ip: &str) -> Answer {
    let mut req = Request::builder()
        .method("POST")
        .uri("/_mm/webhooks/fleet/boot-report")
        .header("content-type", "application/json")
        .header("x-forwarded-for", ip);
    if let Some(a) = authorization {
        req = req.header("authorization", a);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let mut headers: Vec<(String, Vec<u8>)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    headers.sort();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

async fn post(app: &axum::Router, token: Option<&str>, body: String, ip: &str) -> StatusCode {
    send(app, token.map(|t| format!("Bearer {t}")), body, ip)
        .await
        .status
}

/// The node's token is still unspent and no report was stored for it.
async fn assert_untouched(pool: &PgPool, node: &str, what: &str) {
    let spent: bool = sqlx::query_scalar(
        "SELECT used_at IS NOT NULL FROM mm_fleet_boot_tokens WHERE mm_node_id = $1",
    )
    .bind(node)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(!spent, "{what}: the token was spent");
    let stored: bool = sqlx::query_scalar(
        "SELECT boot_report IS NOT NULL FROM mm_fleet_nodes WHERE mm_node_id = $1",
    )
    .bind(node)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(!stored, "{what}: a report was stored");
}

#[tokio::test]
async fn a_report_with_its_token_is_stored_once() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let token = node_with_token(&pool, "tb-report-1", Duration::minutes(15)).await;
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "198.51.100.1").await,
        StatusCode::NO_CONTENT
    );
    let stored: Value = sqlx::query_scalar(
        "SELECT boot_report FROM mm_fleet_nodes WHERE mm_node_id = 'tb-report-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(test_boot::report_of(&stored).unwrap().nvenc, "ok");
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "198.51.100.1").await,
        StatusCode::UNAUTHORIZED,
        "single use"
    );
}

#[tokio::test]
async fn every_token_problem_is_the_same_401() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let expired = node_with_token(&pool, "tb-report-2", Duration::minutes(-1)).await;
    let spent = node_with_token(&pool, "tb-report-2-spent", Duration::minutes(15)).await;
    let live = node_with_token(&pool, "tb-report-2-live", Duration::minutes(15)).await;
    assert_eq!(
        post(&app, Some(&spent), report().to_string(), "198.51.100.2").await,
        StatusCode::NO_CONTENT,
        "spend one token first"
    );

    let cases: Vec<(&str, Option<String>)> = vec![
        ("no header", None),
        ("not a token", Some("Bearer not-a-token".into())),
        ("nothing after the scheme", Some("Bearer ".into())),
        (
            "upper-case hex",
            Some(format!("Bearer {}", live.to_uppercase())),
        ),
        ("63 characters", Some(format!("Bearer {}", &live[1..]))),
        ("never issued", Some(format!("Bearer {}", "0".repeat(64)))),
        ("expired", Some(format!("Bearer {expired}"))),
        ("already spent", Some(format!("Bearer {spent}"))),
        (
            "a live token under another scheme",
            Some(format!("Basic {live}")),
        ),
        ("a live token with no space", Some(format!("Bearer{live}"))),
        ("a live token with no scheme", Some(live.clone())),
    ];
    let mut first: Option<Answer> = None;
    for (what, authorization) in cases {
        let answer = send(&app, authorization, report().to_string(), "198.51.100.2").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{what}");
        match &first {
            None => first = Some(answer),
            Some(f) => assert_eq!(
                &answer, f,
                "{what}: a different answer would tell a caller which problem it has"
            ),
        }
    }
    assert_untouched(&pool, "tb-report-2", "expired").await;
    assert_eq!(
        post(&app, Some(&live), report().to_string(), "198.51.100.2").await,
        StatusCode::NO_CONTENT,
        "none of the refusals above spent the live token"
    );
}

#[tokio::test]
async fn a_bad_body_is_refused_before_the_token_is_spent() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let token = node_with_token(&pool, "tb-report-3", Duration::minutes(15)).await;
    let with = |field: &str, value: Value| {
        let mut r = report();
        r[field] = value;
        r.to_string()
    };
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(5000));
    let cases: Vec<(&str, String, StatusCode)> = vec![
        (
            "an unknown field",
            with("shell", json!("rm -rf /")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "nvenc outside ok/fail",
            with("nvenc", json!("maybe")),
            StatusCode::BAD_REQUEST,
        ),
        ("version 2", with("v", json!(2)), StatusCode::BAD_REQUEST),
        (
            "ok with an error text",
            with("nvenc_error", json!("boom")),
            StatusCode::BAD_REQUEST,
        ),
        (
            "a gpu name of 201 characters",
            with("gpu", json!("g".repeat(201))),
            StatusCode::BAD_REQUEST,
        ),
        (
            "an uptime over a day",
            with("uptime_secs", json!(86_401)),
            StatusCode::BAD_REQUEST,
        ),
        ("not JSON", "not json".into(), StatusCode::BAD_REQUEST),
        ("a JSON array", "[]".into(), StatusCode::BAD_REQUEST),
        ("an empty body", String::new(), StatusCode::BAD_REQUEST),
        ("over 4 KiB", big, StatusCode::PAYLOAD_TOO_LARGE),
    ];
    for (what, body, want) in cases {
        assert_eq!(
            post(&app, Some(&token), body, "198.51.100.3").await,
            want,
            "{what}"
        );
        assert_untouched(&pool, "tb-report-3", what).await;
    }
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "198.51.100.3").await,
        StatusCode::NO_CONTENT,
        "the token is still good"
    );
}

/// `report()` padded with trailing spaces (still valid JSON) to exactly `len` bytes.
fn report_of_len(len: usize) -> String {
    let mut body = report().to_string();
    assert!(body.len() <= len);
    body.push_str(&" ".repeat(len - body.len()));
    body
}

#[tokio::test]
async fn the_limit_is_4096_bytes_exactly() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let token = node_with_token(&pool, "tb-report-4", Duration::minutes(15)).await;
    assert_eq!(test_boot::MAX_REPORT_BYTES, 4096);
    assert_eq!(
        post(&app, Some(&token), report_of_len(4097), "198.51.100.4").await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "one byte over"
    );
    assert_untouched(&pool, "tb-report-4", "one byte over").await;
    assert_eq!(
        post(&app, Some(&token), report_of_len(4096), "198.51.100.4").await,
        StatusCode::NO_CONTENT,
        "exactly at the limit"
    );
}

#[tokio::test]
async fn behind_the_global_middleware_the_limit_is_still_4_kib() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    // The client listener applies a 1 MiB limit to everything; this route's own must still bind.
    let app = app_in_production_stack(pool.clone());
    let token = node_with_token(&pool, "tb-report-5", Duration::minutes(15)).await;
    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(5000));
    assert_eq!(
        post(&app, Some(&token), big, "198.51.100.5").await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_untouched(&pool, "tb-report-5", "over 4 KiB").await;
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "198.51.100.5").await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn one_address_cannot_hammer_it() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = app(pool.clone());
    let token = node_with_token(&pool, "tb-report-6", Duration::minutes(15)).await;
    let mut last = StatusCode::OK;
    for n in 0..31 {
        last = post(&app, None, report().to_string(), "203.0.113.9").await;
        if n < 30 {
            assert_eq!(
                last,
                StatusCode::UNAUTHORIZED,
                "call {n} is within the quota"
            );
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    // The limit is judged before the token: a valid token from the limited address is refused
    // and stays unspent...
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "203.0.113.9").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_untouched(&pool, "tb-report-6", "limited address").await;
    // ...and the limit is per address, not global.
    assert_eq!(
        post(&app, Some(&token), report().to_string(), "203.0.113.10").await,
        StatusCode::NO_CONTENT
    );
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
async fn the_token_and_its_hash_never_reach_a_log_or_a_response() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    // Installed after the file-wide lock is held, before any request: no sibling test is
    // running to cache these callsites as disabled.
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let sink = Capture(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);

    let app = app_in_production_stack(pool.clone());
    let token = node_with_token(&pool, "tb-report-7", Duration::minutes(15)).await;
    let hash = test_boot::token_hash(&token);
    let secrets = [
        token.clone(),
        hex::encode(&hash),
        hex::encode(&hash).to_uppercase(),
        format!("{hash:?}"),
    ];

    // Every path: stored, spent, never issued, a bad body, and a database that fails.
    let mut answers = vec![
        send(
            &app,
            Some(format!("Bearer {token}")),
            report().to_string(),
            "198.51.100.7",
        )
        .await,
        send(
            &app,
            Some(format!("Bearer {token}")),
            report().to_string(),
            "198.51.100.7",
        )
        .await,
        send(
            &app,
            Some(format!("Bearer {}", "1".repeat(64))),
            report().to_string(),
            "198.51.100.7",
        )
        .await,
        send(
            &app,
            Some(format!("Bearer {token}")),
            "{\"nvenc\":\"maybe\"}".into(),
            "198.51.100.7",
        )
        .await,
    ];
    let dead_pool = try_pool().await.unwrap();
    dead_pool.close().await;
    let failing = app_in_production_stack(dead_pool);
    answers.push(
        send(
            &failing,
            Some(format!("Bearer {token}")),
            report().to_string(),
            "198.51.100.7",
        )
        .await,
    );
    assert_eq!(
        answers
            .iter()
            .map(|a| a.status.as_u16())
            .collect::<Vec<_>>(),
        [204, 401, 401, 400, 500],
        "every path was exercised"
    );

    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("test boot report received") && logs.contains("tb-report-7"),
        "the stored report is logged (else this check would pass vacuously): {logs}"
    );
    assert!(
        logs.contains("storing a boot report failed"),
        "the database failure is logged: {logs}"
    );
    for secret in &secrets {
        assert!(!logs.contains(secret.as_str()), "a secret reached the log");
        for a in &answers {
            let body = String::from_utf8_lossy(&a.body);
            let headers = format!("{:?}", a.headers);
            assert!(
                !body.contains(secret.as_str()) && !headers.contains(secret.as_str()),
                "a secret reached a response"
            );
        }
    }
}

/// The real client router (`mm_api::client_router`), which the server mounts: the one place
/// the endpoint is wired in.
async fn real_client_router() -> axum::Router {
    use mm_api::settings_service::{BootOptions, SettingsService};
    use mm_api::state::{AppState, SharedState};
    use tokio_util::sync::CancellationToken;

    // Nothing listens on the discard port: an outbound call is refused at once.
    const DEAD: &str = "http://127.0.0.1:9";
    let database_url = std::env::var("MM_DATABASE_URL").expect("try_pool saw it");
    let db = mm_db::PgDatabase::new(&database_url)
        .await
        .expect("database");
    let mut config = mm_core::config::Config::default();
    config.matrix.homeserver_url = DEAD.into();
    config.sfu.livekit_url = Some(DEAD.into());
    let settings = SettingsService::boot(
        db.pool().clone(),
        config,
        None,
        BootOptions::for_tests(),
        CancellationToken::new(),
    )
    .await
    .expect("settings boot");
    let hs_client = mm_matrix::client::HomeserverClient::new(
        DEAD.into(),
        String::new(),
        "@bot:example.org".into(),
    );
    let signup_pool = db.pool().clone();
    let state: SharedState = Arc::new(AppState {
        sfu: Box::new(mm_sfu::CircuitBreakerAdapter::new(
            mm_sfu::livekit::LiveKitAdapter::new(DEAD.into(), "key".into(), "secret".into(), None),
        )),
        appservice_handler: mm_matrix::appservice::AppserviceHandler::new(hs_client.clone()),
        hs_client,
        db: Box::new(db),
        token_cache: mm_core::cache::TokenCache::default(),
        config_handle: settings.handle().clone(),
        settings,
        metrics: mm_core::metrics::Metrics::new(),
        started_at: std::time::Instant::now(),
        pg_pool: None,
        stripe_client: None,
        payment_registry: None,
        lnurl_client: mm_payment::lnurl::LnurlPayClient::new(),
        entitlement_service: None,
        redis: None,
        ad_engine: None,
        switch_pool: None,
        broadcast_servers: Arc::new(mm_api::broadcast_servers::SnapshotCell::new()),
        ad_switches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
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
    mm_api::client_router(state)
}

/// The probe refuses redirects by design, so the endpoint must answer at exactly
/// `public_url + REPORT_PATH`; and the Stripe webhook, nested under the same prefix, must
/// still be there beside it.
#[tokio::test]
async fn the_client_router_answers_the_report_path_directly() {
    let Some((pool, _g)) = fresh().await else {
        return;
    };
    let app = real_client_router().await;
    let token = node_with_token(&pool, "tb-report-8", Duration::minutes(15)).await;

    let req = Request::builder()
        .method("POST")
        .uri(test_boot::REPORT_PATH)
        .header("content-type", "application/json")
        .header("x-forwarded-for", "198.51.100.8")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(report().to_string()))
        .unwrap();
    let answer = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        answer.status(),
        StatusCode::NO_CONTENT,
        "no redirect, no 404"
    );
    assert!(answer.headers().get("location").is_none());
    let stored: bool = sqlx::query_scalar(
        "SELECT boot_report IS NOT NULL FROM mm_fleet_nodes WHERE mm_node_id = 'tb-report-8'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stored);

    // A trailing slash is a different path: not redirected to the real one (which would carry
    // the token to a place the probe did not ask for), simply not found.
    let req = Request::builder()
        .method("POST")
        .uri(format!("{}/", test_boot::REPORT_PATH))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let answer = app.clone().oneshot(req).await.unwrap();
    assert_eq!(answer.status(), StatusCode::NOT_FOUND);
    assert!(answer.headers().get("location").is_none());

    // The other webhook under the same prefix is still routed (monetization is off here, so
    // it answers 501, not 404).
    let stripe = Request::builder()
        .method("POST")
        .uri("/_mm/webhooks/stripe")
        .body(Body::empty())
        .unwrap();
    let answer = app.oneshot(stripe).await.unwrap();
    assert_ne!(
        answer.status(),
        StatusCode::NOT_FOUND,
        "the stripe webhook is still routed"
    );
}
