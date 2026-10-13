//! Viewer ad events and the creator's own-creative routes, on the real client router.
//!
//! `POST /ads/events`: a skip or a completion ends the ad. The handler drops the
//! scheduled switch-back, switches the viewer onto the programme and removes the ad's
//! source on mm-switch (here a stub that records what it was asked). The short `skip` /
//! `complete` forms used to be refused with 400 before that code ran. Only the viewer
//! the ad was decided for may report on it, or complete it through
//! `POST /streams/{id}/ad-complete`; anyone else gets the unknown-token 404.
//!
//! `GET` / `DELETE /ads/{id}` and `GET /ads/{id}/stats`: a creator reads, deletes and
//! sees the stats of their own creatives and nobody else's. A creative that is not
//! yours answers exactly like one that does not exist (404). That includes platform
//! creatives: they are the operator's, and their stats are an advertiser's.
//!
//! The router needs a full `AppState` against a real Postgres: the tests skip without
//! `MM_DATABASE_URL` and fail loudly with `MM_REQUIRE_DB` set, like the other DB tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{delete, post};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mm_ads::creative::AdSlot;
use mm_api::middleware::AuthConfig;
use mm_api::settings_service::{BootOptions, SettingsService};
use mm_api::state::{AdSwitchEntry, AppState, SharedState};
use mm_api::switch_pool::SwitchPool;
use mm_core::cache::TokenCache;
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::metrics::Metrics;
use mm_core::switch_client::SwitchClient;
use mm_db::PgDatabase;
use mm_db::test_support::require_or_try_pool;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_sfu::CircuitBreakerAdapter;
use mm_sfu::livekit::LiveKitAdapter;

const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";
const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
/// Nothing listens on the discard port, so no outbound call reaches a real service.
const DEAD: &str = "http://127.0.0.1:9";

/// What the stub mm-switch was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SwitchCall {
    Switch { viewer: String, source: String },
    RemoveSource(String),
}

type Calls = Arc<Mutex<Vec<SwitchCall>>>;

struct Server {
    base: String,
    /// The admin router on its own port, as in production.
    admin_base: String,
    state: SharedState,
    pool: PgPool,
    switch_calls: Calls,
}

/// mm-switch holds no viewer whose id starts with this (see [`viewer_left`]).
const GONE: &str = "gone-";
/// mm-switch holds no source whose id starts with this (see [`no_programme_source`]).
const MISSING: &str = "missing-";

/// An mm-switch that answers 200 to a switch and a source removal, and records both. A
/// switch for a [`GONE`] viewer or onto a [`MISSING`] source gets the 404 the real one
/// sends (`http.Error` in `handleSwitch`), which looks the viewer up first.
async fn stub_switch() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let router = Router::new()
        .route(
            "/api/switch",
            post(
                |State(calls): State<Calls>, axum::Json(body): axum::Json<Value>| async move {
                    let viewer = body["viewer_id"].as_str().unwrap_or_default().to_string();
                    let source = body["source_id"].as_str().unwrap_or_default().to_string();
                    calls.lock().unwrap().push(SwitchCall::Switch {
                        viewer: viewer.clone(),
                        source: source.clone(),
                    });
                    let not_found =
                        |what: String| Err((StatusCode::NOT_FOUND, format!("{what} not found\n")));
                    if viewer.starts_with(GONE) {
                        not_found(format!("viewer {viewer}"))
                    } else if source.starts_with(MISSING) {
                        not_found(format!("source {source}"))
                    } else {
                        Ok(axum::Json(json!({ "ok": true })))
                    }
                },
            ),
        )
        .route(
            "/api/sources/{id}",
            delete(
                |State(calls): State<Calls>, Path(id): Path<String>| async move {
                    calls.lock().unwrap().push(SwitchCall::RemoveSource(id));
                    axum::Json(json!({ "ok": true }))
                },
            ),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), calls)
}

async fn start() -> Option<Server> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    let database_url = std::env::var("MM_DATABASE_URL").expect("require_or_try_pool saw it");
    let db = PgDatabase::new(&database_url).await.expect("database");

    let mut config = Config::default();
    config.matrix.homeserver_url = DEAD.into();
    config.sfu.livekit_url = Some(DEAD.into());
    // The settings service imports its base config into the shared database, so boot it
    // with the plain config the other AppState tests use and give the state its own handle:
    // a stored `advertising.enabled = false` must not switch these routes off.
    let settings = SettingsService::boot(
        db.pool().clone(),
        config.clone(),
        None,
        BootOptions::for_tests(),
        CancellationToken::new(),
    )
    .await
    .expect("settings boot");
    config.advertising.enabled = true;
    // The minimum view time is not what these tests are about; with it on, every skip
    // would wait out the default 5 s.
    config.advertising.skip_after_secs = 0;

    let (switch_url, switch_calls) = stub_switch().await;
    let engine =
        mm_ads::AdDecisionEngine::new(config.advertising.clone(), pool.clone(), String::new());

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
        config_handle: ConfigHandle::new(config),
        settings,
        metrics: Metrics::new(),
        started_at: std::time::Instant::now(),
        pg_pool: Some(pool.clone()),
        stripe_client: None,
        payment_registry: None,
        lnurl_client: mm_payment::lnurl::LnurlPayClient::new(),
        entitlement_service: None,
        redis: None,
        ad_engine: Some(Arc::new(engine)),
        switch_pool: Some(Arc::new(SwitchPool::new(Arc::new(SwitchClient::new(
            &switch_url,
        ))))),
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
    let router = mm_api::client_router(state.clone()).layer(axum::Extension(auth.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let admin = mm_api::admin_router(state.clone()).layer(axum::Extension(auth));
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_addr = admin_listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(admin_listener, admin).await.unwrap() });
    Some(Server {
        base: format!("http://{addr}/_mm/client/v1"),
        admin_base: format!("http://{admin_addr}/_mm/admin/v1"),
        state,
        pool,
        switch_calls,
    })
}

fn bearer(user: &str) -> String {
    mm_core::auth::issue_session_token(user, JWT_KEY).unwrap().0
}

/// Fresh ids per test, so tests sharing the database never see each other's rows.
fn user(tag: &str) -> String {
    format!("@ads_{tag}_{}:s", Uuid::new_v4().simple())
}

/// A ready creator-owned creative, written straight to the table.
async fn creative(server: &Server, owner: &str) -> String {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let ad = mm_ads::AdCreative {
        id: id.clone(),
        owner_type: "creator".into(),
        owner_id: owner.into(),
        title: "Test creative".into(),
        placement: "pre_roll".into(),
        duration_secs: 15,
        storage_key: format!("external/{id}"),
        storage_backend: "external".into(),
        cdn_url: Some("https://cdn.example.org/ad.webm".into()),
        mime_type: "video/webm".into(),
        file_size_bytes: 0,
        click_through_url: None,
        categories: json!([]),
        status: "ready".into(),
        created_at: now,
        updated_at: now,
    };
    server
        .state
        .ad_engine
        .as_ref()
        .unwrap()
        .creative_service()
        .create(&ad)
        .await
        .expect("create creative");
    id
}

async fn cleanup(pool: &PgPool, ad_id: &str) {
    sqlx::query("DELETE FROM mm_ad_impressions WHERE ad_id = $1")
        .bind(ad_id)
        .execute(pool)
        .await
        .expect("cleanup impressions");
    sqlx::query("DELETE FROM mm_ad_creatives WHERE id = $1")
        .bind(ad_id)
        .execute(pool)
        .await
        .expect("cleanup creative");
}

/// The state `ad_decision` leaves behind for a viewer it switched onto an ad on the origin:
/// a decided impression and a pending switch-back entry.
struct ServedAd {
    ad_id: String,
    token: String,
    viewer: String,
    viewer_id: String,
    stream_source_id: String,
    ad_source_id: String,
}

async fn serve_ad(server: &Server) -> ServedAd {
    serve_ad_at(server, true, Duration::ZERO).await
}

/// `billable`: the switch was bound to the viewer's node, so `ad_decision` stamped
/// `sfu_revoked_at` (MD1). `ago`: how long the viewer's pipe has carried the ad already.
async fn serve_ad_at(server: &Server, billable: bool, ago: Duration) -> ServedAd {
    let creator = user("creator");
    let viewer = user("viewer");
    let ad_id = creative(server, &creator).await;
    let stream_id = format!("stream-{}", Uuid::new_v4().simple());
    let token = Uuid::new_v4().to_string();
    server
        .state
        .ad_engine
        .as_ref()
        .unwrap()
        .impression_service()
        .record_decision(&token, &ad_id, &stream_id, &viewer, "pre_roll", "creator")
        .await
        .expect("record decision");
    if billable {
        server
            .state
            .ad_engine
            .as_ref()
            .unwrap()
            .impression_service()
            .record_sfu_revoked(&token)
            .await
            .expect("record switch");
    }

    let served = ServedAd {
        ad_id,
        token: token.clone(),
        viewer: viewer.clone(),
        viewer_id: format!("viewer-{}", Uuid::new_v4().simple()),
        stream_source_id: format!("src-{stream_id}"),
        ad_source_id: format!("ad-{}", Uuid::new_v4().simple()),
    };
    server.state.ad_switches.lock().await.insert(
        token,
        AdSwitchEntry {
            viewer_id: served.viewer_id.clone(),
            stream_source_id: served.stream_source_id.clone(),
            ad_source_id: served.ad_source_id.clone(),
            started_at: Instant::now().checked_sub(ago).expect("ago fits the clock"),
            node_id: None,
            slot: AdSlot::PreRoll,
            billable,
        },
    );
    served
}

async fn report(server: &Server, served: &ServedAd, event: &str) -> (StatusCode, Value) {
    report_as(server, &served.viewer, &served.token, event).await
}

async fn report_as(server: &Server, caller: &str, token: &str, event: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/ads/events", server.base))
        .bearer_auth(bearer(caller))
        .json(&json!({ "impression_token": token, "event": event }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn call(
    server: &Server,
    method: reqwest::Method,
    caller: &str,
    path: &str,
) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .request(method, format!("{}{path}", server.base))
        .bearer_auth(bearer(caller))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn get_ad(server: &Server, caller: &str, ad_id: &str) -> (StatusCode, Value) {
    call(
        server,
        reqwest::Method::GET,
        caller,
        &format!("/ads/{ad_id}"),
    )
    .await
}

async fn delete_ad(server: &Server, caller: &str, ad_id: &str) -> (StatusCode, Value) {
    call(
        server,
        reqwest::Method::DELETE,
        caller,
        &format!("/ads/{ad_id}"),
    )
    .await
}

async fn ad_stats(server: &Server, caller: &str, ad_id: &str) -> (StatusCode, Value) {
    call(
        server,
        reqwest::Method::GET,
        caller,
        &format!("/ads/{ad_id}/stats"),
    )
    .await
}

async fn creative_status(pool: &PgPool, ad_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1")
        .bind(ad_id)
        .fetch_one(pool)
        .await
        .expect("read creative status")
}

/// The event ended the ad: the viewer went back to the programme, the ad's source was
/// dropped, and the timer's entry is gone so it will not switch the viewer a second time.
async fn assert_ad_ended(server: &Server, served: &ServedAd, event: &str) {
    let calls = server.switch_calls.lock().unwrap().clone();
    let mine: Vec<_> = calls
        .into_iter()
        .filter(|c| match c {
            SwitchCall::Switch { viewer, .. } => viewer == &served.viewer_id,
            SwitchCall::RemoveSource(id) => id == &served.ad_source_id,
        })
        .collect();
    assert_eq!(
        mine,
        vec![
            SwitchCall::Switch {
                viewer: served.viewer_id.clone(),
                source: served.stream_source_id.clone(),
            },
            SwitchCall::RemoveSource(served.ad_source_id.clone()),
        ],
        "`{event}` must switch the viewer back to the programme, then drop the ad source"
    );
    assert!(
        !server
            .state
            .ad_switches
            .lock()
            .await
            .contains_key(&served.token),
        "`{event}` must take the switch-back off the timer"
    );
}

async fn impression(server: &Server, token: &str) -> mm_ads::AdImpression {
    server
        .state
        .ad_engine
        .as_ref()
        .unwrap()
        .impression_service()
        .get_by_token(token)
        .await
        .expect("read impression")
        .expect("impression exists")
}

/// The viewer's app was killed or lost its connection mid-ad, and mm-switch removed them.
async fn viewer_left(server: &Server, served: &mut ServedAd) {
    served.viewer_id = format!("{GONE}{}", served.viewer_id);
    let mut switches = server.state.ad_switches.lock().await;
    switches
        .get_mut(&served.token)
        .expect("pending switch-back")
        .viewer_id = served.viewer_id.clone();
}

/// The broadcast has no programme source on mm-switch, like a seeded stream with no
/// publisher: the switch-back fails, but the viewer is still connected, on the ad.
async fn no_programme_source(server: &Server, served: &mut ServedAd) {
    served.stream_source_id = format!("{MISSING}{}", served.stream_source_id);
    let mut switches = server.state.ad_switches.lock().await;
    switches
        .get_mut(&served.token)
        .expect("pending switch-back")
        .stream_source_id = served.stream_source_id.clone();
}

/// Everything logged on this test's thread. The router and the stub run on it too, since
/// `#[tokio::test]` is a current-thread runtime.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        let capture = Capture(Arc::default());
        let sink = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        (capture, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

async fn a_skip_event_ends_the_ad(event: &str) {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = report(&server, &served, event).await;

    assert_eq!(status, StatusCode::OK, "`{event}` was refused: {body}");
    assert_ad_ended(&server, &served, event).await;
    assert!(
        impression(&server, &served.token)
            .await
            .skipped_at
            .is_some()
    );
    cleanup(&server.pool, &served.ad_id).await;
}

async fn a_complete_event_ends_the_ad(event: &str) {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = report(&server, &served, event).await;

    assert_eq!(status, StatusCode::OK, "`{event}` was refused: {body}");
    assert_ad_ended(&server, &served, event).await;
    let row = impression(&server, &served.token).await;
    assert!(row.completed_at.is_some());
    assert_eq!(row.quartile_reached, 4);
    cleanup(&server.pool, &served.ad_id).await;
}

#[tokio::test]
async fn skip_switches_the_viewer_back_to_the_programme() {
    a_skip_event_ends_the_ad("skip").await;
}

#[tokio::test]
async fn complete_switches_the_viewer_back_to_the_programme() {
    a_complete_event_ends_the_ad("complete").await;
}

/// What the iOS, Android, web and Flutter players send today.
#[tokio::test]
async fn skipped_switches_the_viewer_back_to_the_programme() {
    a_skip_event_ends_the_ad("skipped").await;
}

#[tokio::test]
async fn completed_switches_the_viewer_back_to_the_programme() {
    a_complete_event_ends_the_ad("completed").await;
}

/// `POST /streams/{id}/ad-complete` as the shipped players send it: an empty challenge
/// response (the proof is not validated yet).
async fn complete_as(server: &Server, caller: &str, token: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/streams/stream-unknown/ad-complete",
            server.base
        ))
        .bearer_auth(bearer(caller))
        .json(&json!({ "impression_token": token, "challenge_response": "", "timestamp": 0 }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_viewer_completes_their_own_ad() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = complete_as(&server, &served.viewer, &served.token).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        impression(&server, &served.token)
            .await
            .completed_at
            .is_some()
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// Completing someone else's ad answers like completing an unknown one, and leaves
/// their impression alone.
#[tokio::test]
async fn completing_another_viewers_ad_is_not_found() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;
    let intruder = user("intruder");
    let unknown = Uuid::new_v4().to_string();

    let (status, body) = complete_as(&server, &intruder, &served.token).await;
    let (_, unknown_body) = complete_as(&server, &intruder, &unknown).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, unknown_body,
        "a foreign impression must answer like an unknown one"
    );
    assert!(
        impression(&server, &served.token)
            .await
            .completed_at
            .is_none()
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// The iOS player reports a skip and then calls `/ad-complete` too (live test 2026-10-13,
/// impression c035901d). An ad ends once: a skipped ad is not also a completion, or every
/// skip inflates the completion rate.
#[tokio::test]
async fn completing_a_skipped_ad_leaves_it_a_skip() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    report(&server, &served, "skipped").await;
    let (status, body) = complete_as(&server, &served.viewer, &served.token).await;

    // Still 200: every shipped player treats the call as best-effort.
    assert_eq!(status, StatusCode::OK, "{body}");
    let row = impression(&server, &served.token).await;
    assert!(row.skipped_at.is_some());
    assert_eq!(
        row.completed_at, None,
        "a skip must not also count as a completion"
    );
    assert!(
        row.quartile_reached < 4,
        "quartile {}",
        row.quartile_reached
    );
    let (_, stats) = admin_stats(&server, &served.ad_id).await;
    assert_eq!(stats["skips"], 1, "{stats}");
    assert_eq!(stats["completions"], 0, "{stats}");
    cleanup(&server.pool, &served.ad_id).await;
}

/// The same rule from the other side: a skip that arrives after the ad completed does not
/// turn the completion into both.
#[tokio::test]
async fn skipping_a_completed_ad_leaves_it_a_completion() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    report(&server, &served, "completed").await;
    let (status, body) = report(&server, &served, "skipped").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let row = impression(&server, &served.token).await;
    assert!(row.completed_at.is_some());
    assert_eq!(
        row.skipped_at, None,
        "a completion must not also count as a skip"
    );
    cleanup(&server.pool, &served.ad_id).await;
}

/// An event on someone else's impression answers like one on an unknown token, and
/// changes nothing: the owner stays on the ad and their impression is not stamped.
#[tokio::test]
async fn another_viewers_skip_is_not_found_and_leaves_their_ad_running() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;
    let intruder = user("intruder");
    let unknown = Uuid::new_v4().to_string();

    let (status, body) = report_as(&server, &intruder, &served.token, "skip").await;
    let (_, unknown_body) = report_as(&server, &intruder, &unknown, "skip").await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, unknown_body,
        "a foreign impression must answer like an unknown one"
    );
    let calls = server.switch_calls.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, SwitchCall::Switch { viewer, .. } if viewer == &served.viewer_id)),
        "another viewer's skip must not switch the owner: {calls:?}"
    );
    assert!(
        server
            .state
            .ad_switches
            .lock()
            .await
            .contains_key(&served.token),
        "another viewer's skip must not take the owner's switch-back off the timer"
    );
    assert!(
        impression(&server, &served.token)
            .await
            .skipped_at
            .is_none()
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// A quartile is progress, not the end: the viewer stays on the ad.
#[tokio::test]
async fn a_quartile_leaves_the_viewer_on_the_ad() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = report(&server, &served, "quartile_50").await;

    assert_eq!(status, StatusCode::OK, "quartile_50 was refused: {body}");
    let calls = server.switch_calls.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, SwitchCall::Switch { viewer, .. } if viewer == &served.viewer_id)),
        "a quartile must not switch the viewer: {calls:?}"
    );
    assert!(
        server
            .state
            .ad_switches
            .lock()
            .await
            .contains_key(&served.token)
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

#[tokio::test]
async fn a_creator_reads_their_own_creative() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;

    let (status, body) = get_ad(&server, &owner, &ad_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], ad_id);
    cleanup(&server.pool, &ad_id).await;
}

/// Another creator's creative is indistinguishable from a missing one, so the read
/// neither leaks the row nor confirms that the id exists.
#[tokio::test]
async fn another_creators_creative_is_not_found() {
    let Some(server) = start().await else { return };
    let ad_id = creative(&server, &user("owner")).await;
    let missing = Uuid::new_v4().to_string();

    let (status, body) = get_ad(&server, &user("other"), &ad_id).await;
    let (missing_status, missing_body) = get_ad(&server, &user("other"), &missing).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, missing_body,
        "a foreign creative must answer like a missing one"
    );
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    cleanup(&server.pool, &ad_id).await;
}

/// A platform creative is the operator's, not any creator's, even if its `owner_id`
/// happens to be the caller's id.
#[tokio::test]
async fn a_platform_creative_is_not_a_creators_to_read() {
    let Some(server) = start().await else { return };
    let caller = user("caller");
    let ad_id = creative(&server, &caller).await;
    sqlx::query("UPDATE mm_ad_creatives SET owner_type = 'platform' WHERE id = $1")
        .bind(&ad_id)
        .execute(&server.pool)
        .await
        .expect("make platform");

    let (status, body) = get_ad(&server, &caller, &ad_id).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn a_creator_deletes_their_own_creative() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;

    let (status, body) = delete_ad(&server, &owner, &ad_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(creative_status(&server.pool, &ad_id).await, "deleted");
    cleanup(&server.pool, &ad_id).await;
}

/// Deleting another creator's creative answers like deleting a missing one, and leaves
/// the creative alone.
#[tokio::test]
async fn deleting_another_creators_creative_is_not_found() {
    let Some(server) = start().await else { return };
    let ad_id = creative(&server, &user("owner")).await;
    let missing = Uuid::new_v4().to_string();

    let (status, body) = delete_ad(&server, &user("other"), &ad_id).await;
    let (_, missing_body) = delete_ad(&server, &user("other"), &missing).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, missing_body,
        "a foreign creative must answer like a missing one"
    );
    assert_eq!(creative_status(&server.pool, &ad_id).await, "ready");
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn a_creator_reads_their_own_creatives_stats() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;

    let (status, body) = ad_stats(&server, &owner, &ad_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ad_id"], ad_id);
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn another_creators_stats_are_not_found() {
    let Some(server) = start().await else { return };
    let ad_id = creative(&server, &user("owner")).await;
    let missing = Uuid::new_v4().to_string();

    let (status, body) = ad_stats(&server, &user("other"), &ad_id).await;
    let (_, missing_body) = ad_stats(&server, &user("other"), &missing).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, missing_body,
        "a foreign creative must answer like a missing one"
    );
    cleanup(&server.pool, &ad_id).await;
}

/// A platform creative's stats are its advertiser's performance across every stream,
/// not any creator's to read.
#[tokio::test]
async fn a_platform_creatives_stats_are_not_found_to_creators() {
    let Some(server) = start().await else { return };
    let ad_id = creative(&server, "admin").await;
    sqlx::query("UPDATE mm_ad_creatives SET owner_type = 'platform' WHERE id = $1")
        .bind(&ad_id)
        .execute(&server.pool)
        .await
        .expect("make platform");
    let missing = Uuid::new_v4().to_string();

    let (status, body) = ad_stats(&server, &user("creator"), &ad_id).await;
    let (_, missing_body) = ad_stats(&server, &user("creator"), &missing).await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body, missing_body,
        "a platform creative must answer like a missing one"
    );
    cleanup(&server.pool, &ad_id).await;
}

// ---------------------------------------------------------------------------
// Measurement (design note 2026-10-11, MD1–MD3)
// ---------------------------------------------------------------------------

async fn report_position(
    server: &Server,
    served: &ServedAd,
    event: &str,
    position_secs: i32,
) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/ads/events", server.base))
        .bearer_auth(bearer(&served.viewer))
        .json(&json!({
            "impression_token": served.token,
            "event": event,
            "position_secs": position_secs,
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn watch_secs(server: &Server, token: &str) -> Option<i32> {
    impression(server, token).await.watch_duration_secs
}

/// One impression row for the stats tests. `billable` stamps `sfu_revoked_at`.
#[derive(Default)]
struct Row {
    billable: bool,
    completed: bool,
    skipped: bool,
    clicked: bool,
    watch: Option<i32>,
}

async fn insert_impression(pool: &PgPool, ad_id: &str, row: Row) {
    sqlx::query(
        "INSERT INTO mm_ad_impressions
            (id, impression_token, ad_id, stream_id, viewer_user_id, slot, owner_type,
             sfu_revoked_at, completed_at, skipped_at, clicked_at, watch_duration_secs)
         VALUES ($1, $2, $3, 'stream-stats', '@stats:s', 'pre_roll', 'creator',
                 CASE WHEN $4 THEN now() END, CASE WHEN $5 THEN now() END,
                 CASE WHEN $6 THEN now() END, CASE WHEN $7 THEN now() END, $8)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(ad_id)
    .bind(row.billable)
    .bind(row.completed)
    .bind(row.skipped)
    .bind(row.clicked)
    .bind(row.watch)
    .execute(pool)
    .await
    .expect("insert impression");
}

async fn admin_stats(server: &Server, ad_id: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .get(format!("{}/ads/{ad_id}/stats", server.admin_base))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// M1: the server knows when it switched the viewer onto the ad and when it switched
/// them back. That interval is the watch time, and no client can fake or block it.
#[tokio::test]
async fn a_skip_records_how_long_the_pipe_carried_the_ad() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, true, Duration::from_secs(7)).await;

    let (status, body) = report(&server, &served, "skip").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let watched = watch_secs(&server, &served.token).await;
    assert!(
        matches!(watched, Some(7 | 8)),
        "expected the 7 s the pipe carried the ad, got {watched:?}"
    );
    cleanup(&server.pool, &served.ad_id).await;
}

/// The switch-back can come after the creative ended (the timer's grace, a viewer frozen
/// on the last frame). Only the creative's own length counts.
#[tokio::test]
async fn server_watch_time_never_exceeds_the_creative() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, true, Duration::from_secs(40)).await;

    let (status, body) = report(&server, &served, "completed").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(watch_secs(&server, &served.token).await, Some(15));
    cleanup(&server.pool, &served.ad_id).await;
}

/// MD1: a switch that could not be bound to the viewer's node was never shown, so the
/// server has nothing to attest.
#[tokio::test]
async fn an_unbillable_switch_records_no_server_watch_time() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, false, Duration::from_secs(7)).await;

    let (status, body) = report(&server, &served, "skip").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(watch_secs(&server, &served.token).await, None);
    cleanup(&server.pool, &served.ad_id).await;
}

/// The safety-net timer, for a viewer still on the ad: the time counts up to the creative's
/// length, the viewer goes back to the programme, and it is not a completion (MD2).
#[tokio::test]
async fn the_timer_attests_watch_time_but_no_completion() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, true, Duration::from_secs(40)).await;

    mm_api::ads::switch_back_on_timer(&server.state, &served.token).await;

    assert_ad_ended(&server, &served, "the timer").await;
    let row = impression(&server, &served.token).await;
    assert_eq!(row.watch_duration_secs, Some(15));
    assert_eq!(row.completed_at, None);
    cleanup(&server.pool, &served.ad_id).await;
}

/// Live test 2026-10-13, impression 4c85c1b5: the app was killed about 6 s into a 25 s ad,
/// and the timer at 40 s credited all 25. mm-switch had already removed the viewer and
/// keeps no record of when they left, so the server cannot attest any watch time.
#[tokio::test]
async fn the_timer_attests_no_watch_time_for_a_viewer_who_left() {
    let Some(server) = start().await else { return };
    let mut served = serve_ad_at(&server, true, Duration::from_secs(40)).await;
    viewer_left(&server, &mut served).await;

    mm_api::ads::switch_back_on_timer(&server.state, &served.token).await;

    assert_eq!(watch_secs(&server, &served.token).await, None);
    cleanup(&server.pool, &served.ad_id).await;
}

/// The same on the skip path: a switch-back that finds the viewer gone attests nothing.
#[tokio::test]
async fn a_skip_from_a_viewer_who_left_attests_no_watch_time() {
    let Some(server) = start().await else { return };
    let mut served = serve_ad_at(&server, true, Duration::from_secs(7)).await;
    viewer_left(&server, &mut served).await;

    let (status, body) = report(&server, &served, "skip").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(watch_secs(&server, &served.token).await, None);
    cleanup(&server.pool, &served.ad_id).await;
}

/// Live test 2026-10-13, run A: the seeded stream has no programme source, so the
/// switch-back fails with source-not-found. mm-switch found the viewer before it looked
/// for the source, so the viewer was still on the ad and the time counts.
#[tokio::test]
async fn a_switch_back_without_a_programme_source_still_attests_watch_time() {
    let Some(server) = start().await else { return };
    let mut served = serve_ad_at(&server, true, Duration::from_secs(7)).await;
    no_programme_source(&server, &mut served).await;

    let (status, body) = report(&server, &served, "skip").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let watched = watch_secs(&server, &served.token).await;
    assert!(
        matches!(watched, Some(7 | 8)),
        "expected the 7 s the pipe carried the ad, got {watched:?}"
    );
    cleanup(&server.pool, &served.ad_id).await;
}

/// "Ad ended, viewer back to stream" was logged right after a switch-back that failed.
#[tokio::test]
async fn a_failed_switch_back_is_logged_as_a_failure() {
    let Some(server) = start().await else { return };
    let mut served = serve_ad(&server).await;
    no_programme_source(&server, &mut served).await;
    let (log, _guard) = Capture::install();

    report(&server, &served, "skip").await;

    let log = log.text();
    assert!(log.contains("switch-back to the stream failed"), "{log}");
    assert!(!log.contains("back to stream"), "{log}");
    cleanup(&server.pool, &served.ad_id).await;
}

/// The timer's "Ad ended (timer), back to stream" had the same problem.
#[tokio::test]
async fn a_failed_switch_back_on_the_timer_is_logged_as_a_failure() {
    let Some(server) = start().await else { return };
    let mut served = serve_ad(&server).await;
    viewer_left(&server, &mut served).await;
    let (log, _guard) = Capture::install();

    mm_api::ads::switch_back_on_timer(&server.state, &served.token).await;

    let log = log.text();
    assert!(log.contains("switch-back to the stream failed"), "{log}");
    assert!(!log.contains("back to stream"), "{log}");
    cleanup(&server.pool, &served.ad_id).await;
}

/// M2: where the server cannot attest, the furthest position the client reported is the
/// fallback, clamped to the creative.
#[tokio::test]
async fn client_positions_keep_the_furthest_point_within_the_creative() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, false, Duration::ZERO).await;

    report_position(&server, &served, "quartile_50", 9).await;
    assert_eq!(watch_secs(&server, &served.token).await, Some(9));

    report_position(&server, &served, "quartile_25", 4).await;
    assert_eq!(
        watch_secs(&server, &served.token).await,
        Some(9),
        "an earlier position must not lower the watch time"
    );

    report_position(&server, &served, "quartile_75", 99).await;
    assert_eq!(watch_secs(&server, &served.token).await, Some(15));

    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// A server value always wins: a client position never touches an attested impression,
/// before or after the switch-back.
#[tokio::test]
async fn a_client_position_never_touches_an_attested_impression() {
    let Some(server) = start().await else { return };
    let served = serve_ad_at(&server, true, Duration::from_secs(7)).await;

    report_position(&server, &served, "quartile_50", 12).await;
    assert_eq!(watch_secs(&server, &served.token).await, None);

    report(&server, &served, "skip").await;
    report_position(&server, &served, "quartile_75", 14).await;
    let watched = watch_secs(&server, &served.token).await;
    assert!(
        matches!(watched, Some(7 | 8)),
        "the server's 7 s must stand, got {watched:?}"
    );
    cleanup(&server.pool, &served.ad_id).await;
}

/// M4 + MD1: an impression is a billable switch, not a decision. Completions, skips and
/// clicks are counted among impressions, so the rates stay rates.
#[tokio::test]
async fn stats_count_delivered_impressions_not_decisions() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;
    let pool = &server.pool;
    // Two decisions that were never shown; one of them still reported a completion.
    insert_impression(pool, &ad_id, Row::default()).await;
    insert_impression(
        pool,
        &ad_id,
        Row {
            completed: true,
            ..Row::default()
        },
    )
    .await;
    // Three delivered impressions.
    insert_impression(
        pool,
        &ad_id,
        Row {
            billable: true,
            completed: true,
            watch: Some(15),
            ..Row::default()
        },
    )
    .await;
    insert_impression(
        pool,
        &ad_id,
        Row {
            billable: true,
            skipped: true,
            watch: Some(5),
            ..Row::default()
        },
    )
    .await;
    insert_impression(
        pool,
        &ad_id,
        Row {
            billable: true,
            clicked: true,
            ..Row::default()
        },
    )
    .await;

    let (status, creator) = ad_stats(&server, &owner, &ad_id).await;
    let (admin_status, admin) = admin_stats(&server, &ad_id).await;

    assert_eq!(status, StatusCode::OK, "{creator}");
    assert_eq!(admin_status, StatusCode::OK, "{admin}");
    for body in [&creator, &admin] {
        assert_eq!(body["total_impressions"], 3, "{body}");
        assert_eq!(body["decisions"], 5, "{body}");
        assert_eq!(body["completions"], 1, "{body}");
        assert_eq!(body["skips"], 1, "{body}");
        assert_eq!(body["clicks"], 1, "{body}");
        assert_eq!(body["avg_watch_secs"], 10.0, "{body}");
    }
    let third = 1.0 / 3.0;
    assert!(
        (creator["completion_rate"].as_f64().unwrap() - third).abs() < 1e-9,
        "{creator}"
    );
    assert!(
        (creator["ctr"].as_f64().unwrap() - third).abs() < 1e-9,
        "{creator}"
    );
    cleanup(pool, &ad_id).await;
}

/// M5: the creator's stats used to read 1000 rows and count them.
#[tokio::test]
async fn creator_stats_are_not_capped_at_1000_impressions() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;
    sqlx::query(
        "INSERT INTO mm_ad_impressions
            (id, impression_token, ad_id, stream_id, viewer_user_id, slot, owner_type, sfu_revoked_at)
         SELECT gen_random_uuid()::text, gen_random_uuid()::text, $1, 'stream-cap', '@cap:s',
                'pre_roll', 'creator', now()
         FROM generate_series(1, 1005)",
    )
    .bind(&ad_id)
    .execute(&server.pool)
    .await
    .expect("insert 1005 impressions");

    let (status, body) = ad_stats(&server, &owner, &ad_id).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_impressions"], 1005, "{body}");
    cleanup(&server.pool, &ad_id).await;
}
