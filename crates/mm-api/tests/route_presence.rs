//! Route presence (ADR-0011 section 5.1, audit item 6b).
//!
//! The REST compatibility check only sees the routes documented in the
//! OpenAPI files; the routers register many more. This test sends one request
//! for every route in `tests/client_routes.txt` (the routes the shipped apps
//! and the web packages call) through the real routers, assembled exactly as
//! `mm-server` does, and fails when the router itself answers "no such route"
//! (404) or "no such method" (405) with axum's empty body. A handler that
//! answers 401, 400, 404 with a JSON error, 500 or 501 proves the route is
//! still registered.
//!
//! No external service: the database is an in-memory SQLite, the Postgres
//! pool is lazy and never reaches a server (127.0.0.1:9), and the homeserver
//! and SFU are unreachable addresses.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use mm_api::middleware::AuthConfig;
use mm_api::settings_service::SettingsService;
use mm_api::state::{AppState, SharedState};
use mm_core::cache::TokenCache;
use mm_core::config::Config;
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_db::sqlite::SqliteDatabase;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_sfu::CircuitBreakerAdapter;
use mm_sfu::livekit::LiveKitAdapter;

const DEAD: &str = "http://127.0.0.1:9";

async fn routers() -> axum::Router {
    let mut config = Config::default();
    config.matrix.homeserver_url = DEAD.into();
    config.sfu.livekit_url = Some(DEAD.into());
    config.server.feed_enabled = true;
    let db = SqliteDatabase::new(&format!(
        "sqlite:file:routes_{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://unused@127.0.0.1:9/unused")
        .expect("lazy pool");
    let settings = SettingsService::without_database(pool.clone(), config.clone());
    let hs_client = HomeserverClient::new(DEAD.into(), String::new(), "@mmbot:example.org".into());
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
        switch_pool: None,
        broadcast_servers: Arc::new(mm_api::broadcast_servers::SnapshotCell::new()),
        ad_switches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        signup_limiter: mm_api::rate_limit::LiveQuotaLimiter::new(5),
        signup_avail_limiter: mm_api::rate_limit::SignupRateLimiter::new(60),
        synapse_admin: Arc::new(mm_core::synapse_admin::SynapseAdminClient::new(
            DEAD,
            String::new(),
        )),
        signup_pool: pool,
        announcement_cache: Arc::new(moka::future::Cache::builder().max_capacity(1).build()),
        feed_cache: Arc::new(moka::future::Cache::builder().max_capacity(10).build()),
        feed_limiter: mm_api::rate_limit::SignupRateLimiter::new(1800),
        moderation_report_limiter: mm_api::rate_limit::SignupRateLimiter::new(10),
        permissions_cache: Arc::new(moka::future::Cache::builder().max_capacity(10).build()),
        trending_engine: None,
    });
    let auth = AuthConfig {
        jwt_signing_key: "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF".into(),
        admin_token: "admin-token-0123456789abcdef0123456789abcdef".into(),
        hs_token: String::new(),
        matrix_homeserver_url: DEAD.into(),
    };
    // As mm-server builds the client port (startup.rs, "Build routers").
    let app = mm_api::client_router(state.clone())
        .merge(mm_api::wellknown::routes(state.clone()))
        .merge(mm_api::readyz::routes(state.clone()));
    mm_api::middleware::apply_middleware(app, state.config_handle.clone(), auth)
}

fn listed_routes() -> Vec<(Method, String)> {
    let text = include_str!("client_routes.txt");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (m, p) = l.split_once(' ').expect("METHOD PATH");
            (m.parse::<Method>().expect("method"), p.trim().to_string())
        })
        .collect()
}

/// axum's own "no route" / "wrong method" answers: 404 or 405 with no body.
async fn router_refused(app: &axum::Router, method: Method, path: &str) -> Option<StatusCode> {
    let uri = path.replace('!', "%21").replace('@', "%40");
    let has_body = matches!(method, Method::POST | Method::PUT);
    let mut req = Request::builder().method(method).uri(uri);
    if has_body {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(if has_body {
            Body::from("{}")
        } else {
            Body::empty()
        })
        .unwrap();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), app.clone().oneshot(req))
        .await
        .expect("answered in time")
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    ((status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED)
        && body.is_empty())
    .then_some(status)
}

#[tokio::test]
async fn every_route_the_clients_call_is_still_registered() {
    let app = routers().await;
    let routes = listed_routes();
    assert!(routes.len() > 80, "the list has {} routes", routes.len());
    let mut missing = Vec::new();
    for (method, path) in routes {
        if let Some(status) = router_refused(&app, method.clone(), &path).await {
            missing.push(format!("{method} {path} -> {status}"));
        }
    }
    assert!(
        missing.is_empty(),
        "routes no longer registered:\n  {}",
        missing.join("\n  ")
    );
}

/// The check can fail: an unregistered route and a wrong method are caught.
#[tokio::test]
async fn the_check_detects_a_missing_route_and_a_wrong_method() {
    let app = routers().await;
    assert_eq!(
        router_refused(&app, Method::GET, "/_mm/client/v1/no-such-route").await,
        Some(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        router_refused(&app, Method::DELETE, "/_mm/client/v1/feed").await,
        Some(StatusCode::METHOD_NOT_ALLOWED)
    );
}
