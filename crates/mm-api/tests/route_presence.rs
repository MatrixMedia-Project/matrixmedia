//! Route presence (ADR-0011 section 5.1).
//!
//! The REST compatibility check only sees the routes documented in the
//! OpenAPI files; the routers register many more. For every route template in
//! `tests/client_routes.txt` (the routes the shipped apps and the web packages
//! call) this test sends one request, with each `{param}` filled by a sample
//! value, through the client-port routes exactly as mm-server mounts them
//! (`mm_api::client_port_router`, which mm-server's startup calls), and
//! asserts the router matched **that template** (axum's `MatchedPath`). So a
//! removed `GET /streams/active-mine` fails even though the request would
//! still match `GET /streams/{id}`, and a wrong method or unknown path fails.
//!
//! No external service: the database is an in-memory SQLite, the Postgres
//! pool is lazy and never reaches a server (127.0.0.1:9), and the homeserver
//! and SFU are unreachable addresses.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::{HeaderValue, Method, Request, StatusCode};
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
    // The same function mm-server's startup mounts on the client port.
    let app = mm_api::client_port_router(state.clone(), None);
    record_matched_path(mm_api::middleware::apply_middleware(
        app,
        state.config_handle.clone(),
        auth,
    ))
}

const MATCHED: &str = "x-test-matched-path";

/// Echo the route template axum matched in a response header (a layer added
/// with `Router::layer` runs after routing and sees `MatchedPath`).
fn record_matched_path(app: axum::Router) -> axum::Router {
    app.layer(axum::middleware::from_fn(
        |req: axum::extract::Request, next: axum::middleware::Next| async move {
            let matched = req
                .extensions()
                .get::<MatchedPath>()
                .map(|m| m.as_str().to_owned());
            let mut resp = next.run(req).await;
            if let Some(m) = matched.and_then(|m| HeaderValue::from_str(&m).ok()) {
                resp.headers_mut().insert(MATCHED, m);
            }
            resp
        },
    ))
}

/// A sample value for a path parameter.
fn sample(param: &str) -> &'static str {
    match param {
        "room_id" | "matrix_room_id" => "%21r%3Aexample.org",
        "creator_id" | "user_id" => "%40alice%3Aexample.org",
        _ => "x1",
    }
}

/// Fill every `{param}` of `template` with a sample value.
fn concrete(template: &str) -> String {
    template
        .split('/')
        .map(
            |seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(param) => sample(param).to_string(),
                None => seg.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
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

/// Send one request for `template` and say how the router handled it: `Ok`
/// when it matched that very template with that method, `Err` otherwise.
async fn check_route(app: &axum::Router, method: Method, template: &str) -> Result<(), String> {
    let has_body = matches!(method, Method::POST | Method::PUT);
    let mut req = Request::builder()
        .method(method.clone())
        .uri(concrete(template));
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
    let matched = resp
        .headers()
        .get(MATCHED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match matched {
        _ if status == StatusCode::METHOD_NOT_ALLOWED => Err(format!(
            "{method} {template}: method not allowed ({status})"
        )),
        Some(m) if m == template => Ok(()),
        Some(m) => Err(format!(
            "{method} {template}: routed to {m} instead ({status})"
        )),
        None => Err(format!("{method} {template}: no route matched ({status})")),
    }
}

#[tokio::test]
async fn every_route_the_clients_call_is_still_registered() {
    let app = routers().await;
    let routes = listed_routes();
    assert!(routes.len() >= 90, "the list has {} routes", routes.len());
    let mut problems = Vec::new();
    for (method, template) in routes {
        if let Err(e) = check_route(&app, method, &template).await {
            problems.push(e);
        }
    }
    assert!(
        problems.is_empty(),
        "routes the clients call are gone or shadowed:\n  {}",
        problems.join("\n  ")
    );
}

/// The check fails for an unknown path, a wrong method, and a route that is
/// gone but whose request would still be taken by a broader template (the
/// shadowed case: without `/streams/active-mine`, `/streams/{id}` answers).
#[tokio::test]
async fn the_check_detects_a_missing_route_a_wrong_method_and_a_shadowed_route() {
    let app = routers().await;
    assert!(
        check_route(&app, Method::GET, "/_mm/client/v1/no-such-route")
            .await
            .is_err()
    );
    assert!(
        check_route(&app, Method::DELETE, "/_mm/client/v1/feed")
            .await
            .is_err()
    );

    // A router that registers only the broader template.
    let shadowed = record_matched_path(axum::Router::new().route(
        "/_mm/client/v1/streams/{id}",
        axum::routing::get(|| async { "s" }),
    ));
    let err = check_route(&shadowed, Method::GET, "/_mm/client/v1/streams/active-mine")
        .await
        .expect_err("a shadowed route must fail");
    assert!(
        err.contains("routed to /_mm/client/v1/streams/{id}"),
        "{err}"
    );
    // ...and the broader template itself still passes there.
    assert!(
        check_route(&shadowed, Method::GET, "/_mm/client/v1/streams/{id}")
            .await
            .is_ok()
    );
}
