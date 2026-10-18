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
//! `POST /ads`, `PUT` / `DELETE /admin/v1/ads/{id}` and the admin list: ads are drafts until
//! their upload is converted, `PUT` only pauses and resumes, a placement change cannot put a
//! converted ad over its slot's cap, a creator is held to `max_ads_per_creator`, and a delete
//! cancels the open conversion.
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

use mm_ads::conversion::{RenditionKind, Warning};
use mm_ads::conversion_db::{self as q, NewRendition, Publication, PublishOutcome};
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
    stream_id: String,
    stream_source_id: String,
    ad_source_id: String,
    /// Stored HMAC challenge (`None` for legacy rows decided without one).
    challenge_nonce: Option<String>,
    viewer_secret: Option<String>,
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
    let challenge = mm_ads::enforcement::generate_challenge(&ad_id);
    server
        .state
        .ad_engine
        .as_ref()
        .unwrap()
        .impression_service()
        .record_decision(
            &token,
            &ad_id,
            &stream_id,
            &viewer,
            "pre_roll",
            "creator",
            Some((&challenge.nonce, &challenge.viewer_secret)),
        )
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
        stream_id: stream_id.clone(),
        stream_source_id: format!("src-{stream_id}"),
        ad_source_id: format!("ad-{}", Uuid::new_v4().simple()),
        challenge_nonce: Some(challenge.nonce),
        viewer_secret: Some(challenge.viewer_secret),
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

/// `POST /streams/{id}/ad-complete` with a valid HMAC proof computed from the
/// served challenge, exactly as an honest player sends it.
async fn complete_as(server: &Server, caller: &str, served: &ServedAd) -> (StatusCode, Value) {
    let ts = chrono::Utc::now().timestamp();
    let response = mm_ads::enforcement::compute_expected_response(
        served.challenge_nonce.as_deref().unwrap(),
        &served.ad_id,
        ts,
        served.viewer_secret.as_deref().unwrap(),
    );
    complete_raw(server, caller, &served.stream_id, &served.token, &response, ts).await
}

/// Raw completion call for negative cases (bad/empty proofs, wrong stream).
async fn complete_raw(
    server: &Server,
    caller: &str,
    stream_id: &str,
    token: &str,
    response: &str,
    timestamp: i64,
) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/streams/{stream_id}/ad-complete", server.base))
        .bearer_auth(bearer(caller))
        .json(&json!({ "impression_token": token, "challenge_response": response, "timestamp": timestamp }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A legacy impression decided without a stored challenge (pre-V045 row).
async fn serve_legacy_ad(server: &Server) -> ServedAd {
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
        .record_decision(&token, &ad_id, &stream_id, &viewer, "pre_roll", "creator", None)
        .await
        .expect("record legacy decision");
    ServedAd {
        ad_id,
        token,
        viewer,
        viewer_id: format!("viewer-{}", Uuid::new_v4().simple()),
        stream_id,
        stream_source_id: String::new(),
        ad_source_id: String::new(),
        challenge_nonce: None,
        viewer_secret: None,
    }
}

#[tokio::test]
async fn a_viewer_completes_their_own_ad() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = complete_as(&server, &served.viewer, &served).await;

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

/// A forged or empty proof on a challenged impression is rejected and stamps nothing.
#[tokio::test]
async fn completing_with_bad_or_empty_proof_is_forbidden() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;
    let ts = chrono::Utc::now().timestamp();

    for response in [
        "0".repeat(64),
        String::new(),
        "short".to_string(),
    ] {
        let (status, body) = complete_raw(
            &server,
            &served.viewer,
            &served.stream_id,
            &served.token,
            &response,
            ts,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    assert!(
        impression(&server, &served.token)
            .await
            .completed_at
            .is_none(),
        "rejected proofs must not complete the impression"
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// A proof minted for another stream does not complete this one.
#[tokio::test]
async fn completing_on_the_wrong_stream_is_not_found() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (status, body) = complete_raw(
        &server,
        &served.viewer,
        "stream-unknown",
        &served.token,
        "00",
        chrono::Utc::now().timestamp(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        impression(&server, &served.token)
            .await
            .completed_at
            .is_none()
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// Replaying a valid completion stays 200 but changes nothing further.
#[tokio::test]
async fn replaying_a_completion_is_idempotent() {
    let Some(server) = start().await else { return };
    let served = serve_ad(&server).await;

    let (first, _) = complete_as(&server, &served.viewer, &served).await;
    assert_eq!(first, StatusCode::OK);
    let stamped = impression(&server, &served.token).await.completed_at;
    assert!(stamped.is_some());

    let (second, body) = complete_as(&server, &served.viewer, &served).await;
    assert_eq!(second, StatusCode::OK, "{body}");
    assert_eq!(
        impression(&server, &served.token).await.completed_at,
        stamped,
        "replay must not re-stamp the completion"
    );
    server.state.ad_switches.lock().await.remove(&served.token);
    cleanup(&server.pool, &served.ad_id).await;
}

/// Pre-V045 rows without a stored challenge take the legacy accept-once path.
#[tokio::test]
async fn legacy_impression_without_challenge_still_completes() {
    let Some(server) = start().await else { return };
    let served = serve_legacy_ad(&server).await;

    let (status, body) = complete_raw(
        &server,
        &served.viewer,
        &served.stream_id,
        &served.token,
        "",
        0,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        impression(&server, &served.token)
            .await
            .completed_at
            .is_some()
    );
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

    let (status, body) = complete_raw(&server, &intruder, &served.stream_id, &served.token, "", 0).await;
    let (_, unknown_body) = complete_raw(&server, &intruder, &served.stream_id, &unknown, "", 0).await;

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
    let (status, body) = complete_as(&server, &served.viewer, &served).await;

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

// ---------------------------------------------------------------------------
// The lifecycle on the JSON endpoints
// ---------------------------------------------------------------------------

/// Jobs are claimed from one shared queue, so the tests that run a conversion take turns.
static PIPELINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The admin router on the server's state, on a port of its own.
async fn admin_base(server: &Server) -> String {
    let auth = AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    let router = mm_api::admin_router(server.state.clone()).layer(axum::Extension(auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/_mm/admin/v1")
}

async fn admin_call(
    base: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(ADMIN_TOKEN);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn create_ad(server: &Server, caller: &str, body: Value) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/ads", server.base))
        .bearer_auth(bearer(caller))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn live_ads(server: &Server, owner: &str) -> i64 {
    server
        .state
        .ad_engine
        .as_ref()
        .unwrap()
        .creative_service()
        .count_live_by_owner("creator", owner)
        .await
        .unwrap()
}

/// Replaces the live config, as a dashboard edit does.
fn set_config(server: &Server, edit: impl FnOnce(&mut Config)) {
    let mut config = (*server.state.config()).clone();
    edit(&mut config);
    server.state.config_handle.store(config);
}

/// A minimal MP4 `ftyp` header, padded: all the intake sniffs for.
fn mp4(len: usize) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm'];
    v.resize(len.max(12), 0x42);
    v
}

async fn upload(url: &str, bearer: &str, file: &[u8]) -> (StatusCode, Value) {
    let boundary = "mmboundary7";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ad.mp4\"\r\nContent-Type: video/mp4\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(bearer)
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .body(body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// Gives the intake a media directory of its own and clears the shared queue.
async fn pipeline_dir(server: &Server) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().display().to_string();
    set_config(server, |c| c.advertising.media_dir = path);
    sqlx::query("DELETE FROM mm_ad_conversion_jobs")
        .execute(&server.pool)
        .await
        .unwrap();
    dir
}

async fn job_states(pool: &PgPool, ad_id: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT state FROM mm_ad_conversion_jobs WHERE creative_id = $1 ORDER BY created_at")
        .bind(ad_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_creator_creates_a_draft_and_cannot_supply_a_media_url() {
    let Some(server) = start().await else { return };
    let owner = user("owner");

    let (status, body) = create_ad(
        &server,
        &owner,
        json!({ "title": "t", "placement": "pre_roll", "media_url": "https://cdn.example.org/a.mp4" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("media_url"), "{body}");
    assert_eq!(live_ads(&server, &owner).await, 0, "a refused create leaves nothing");

    // A null media_url is no media_url; a duration the client claims is not believed.
    let (status, body) = create_ad(
        &server,
        &owner,
        json!({ "title": "t", "placement": "pre_roll", "media_url": null, "duration_secs": 99 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["status"].as_str(), body["duration_secs"].as_i64(), body["media_url"].as_str()),
        (Some("draft"), Some(15), Some(""))
    );
    let ad_id = body["id"].as_str().unwrap().to_string();
    let (status, cdn, duration, backend, mime): (String, Option<String>, i32, String, String) =
        sqlx::query_as(
            "SELECT status, cdn_url, duration_secs, storage_backend, mime_type FROM mm_ad_creatives WHERE id = $1",
        )
        .bind(&ad_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(
        (status.as_str(), cdn, duration, backend.as_str(), mime.as_str()),
        ("draft", None, 15, "local", "video/webm")
    );
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn a_creator_is_held_to_max_ads_per_creator() {
    let Some(server) = start().await else { return };
    set_config(&server, |c| c.advertising.max_ads_per_creator = 2);
    let owner = user("owner");
    let ad = || json!({ "title": "t", "placement": "pre_roll" });

    let (status, first) = create_ad(&server, &owner, ad()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = create_ad(&server, &owner, ad()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let (status, third) = create_ad(&server, &owner, ad()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{third}");
    assert_eq!(
        third["message"], "You already have 2 ads; the limit is 2. Delete one first.",
        "{third}"
    );
    assert_eq!(live_ads(&server, &owner).await, 2);

    let (status, theirs) = create_ad(&server, &user("other"), ad()).await;
    assert_eq!(status, StatusCode::OK, "the limit is per creator: {theirs}");

    // A deleted ad no longer counts, so deleting one frees its slot.
    let first_id = first["id"].as_str().unwrap();
    let (status, _) = delete_ad(&server, &owner, first_id).await;
    assert_eq!(status, StatusCode::OK);
    let (status, fourth) = create_ad(&server, &owner, ad()).await;
    assert_eq!(status, StatusCode::OK, "{fourth}");

    // 0 means no limit.
    set_config(&server, |c| c.advertising.max_ads_per_creator = 0);
    let (status, fifth) = create_ad(&server, &owner, ad()).await;
    assert_eq!(status, StatusCode::OK, "{fifth}");

    for body in [&first, &second, &theirs, &fourth, &fifth] {
        cleanup(&server.pool, body["id"].as_str().unwrap()).await;
    }
}

#[tokio::test]
async fn the_admin_creates_a_draft_and_cannot_supply_a_cdn_url() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let title = format!("promo-{}", Uuid::new_v4().simple());

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": title, "placement": "mid_roll", "cdn_url": "https://cdn.example.org/a.mp4" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("cdn_url"), "{body}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_creatives WHERE title = $1")
        .bind(&title)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "a refused create leaves nothing");

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": title, "placement": "mid_roll", "cdn_url": null, "duration_secs": 99 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "draft");
    let ad_id = body["id"].as_str().unwrap().to_string();
    let (owner_type, placement, status, cdn, duration): (String, String, String, Option<String>, i32) =
        sqlx::query_as(
            "SELECT owner_type, placement, status, cdn_url, duration_secs FROM mm_ad_creatives WHERE id = $1",
        )
        .bind(&ad_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(
        (owner_type.as_str(), placement.as_str(), status.as_str(), cdn, duration),
        ("platform", "mid_roll", "draft", None, 15)
    );

    // The list shows the draft with no conversion yet.
    let (status, list) = admin_call(&admin, reqwest::Method::GET, "/ads", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let item = list["ads"].as_array().unwrap().iter().find(|a| a["id"] == ad_id.as_str()).expect("listed");
    assert_eq!(item["status"], "draft");
    assert_eq!(item["preview_url"], Value::Null);
    assert_eq!(item["media"], json!({ "job": null, "vp8": null, "media_warnings": [] }));
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn an_admin_put_only_pauses_and_resumes() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let (_, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": "before", "placement": "pre_roll" })),
    )
    .await;
    let ad_id = body["id"].as_str().unwrap().to_string();
    let put = |body: Value| {
        let (admin, path) = (admin.clone(), format!("/ads/{ad_id}"));
        async move { admin_call(&admin, reqwest::Method::PUT, &path, Some(body)).await }
    };

    // What conversion sets cannot be edited; null means absent.
    for field in ["cdn_url", "duration_secs"] {
        let value = if field == "cdn_url" { json!("https://cdn.example.org/a.mp4") } else { json!(10) };
        let (status, body) = put(json!({ field: value })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert!(body["message"].as_str().unwrap().contains(field), "{body}");
    }
    let (status, body) = put(json!({ "cdn_url": null, "title": "after" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (title, cdn): (String, Option<String>) =
        sqlx::query_as("SELECT title, cdn_url FROM mm_ad_creatives WHERE id = $1")
            .bind(&ad_id)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!((title.as_str(), cdn), ("after", None));

    // Only paused and ready can be asked for, and ready needs converted media.
    for status_wanted in ["deleted", "processing", "draft", "failed", "nonsense"] {
        let (status, body) = put(json!({ "status": status_wanted })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{status_wanted}: {body}");
        assert!(body["message"].as_str().unwrap().contains("cannot be set"), "{body}");
        assert_eq!(creative_status(&server.pool, &ad_id).await, "draft");
    }
    let (status, body) = put(json!({ "status": "ready" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["message"], "This ad has no converted media yet (it is draft).");
    assert_eq!(creative_status(&server.pool, &ad_id).await, "draft");

    let (status, body) = put(json!({ "status": "paused" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(creative_status(&server.pool, &ad_id).await, "paused");

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &format!("/ads/{}", Uuid::new_v4()),
        Some(json!({ "status": "paused" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // An unknown id is a 404 whatever the body asks for.
    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &format!("/ads/{}", Uuid::new_v4()),
        Some(json!({ "title": "nobody" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    cleanup(&server.pool, &ad_id).await;
}

/// Spec §5.4: a deleted ad takes no pause, no resume and no edit; the answer is a 409 that says why.
#[tokio::test]
async fn a_deleted_ad_answers_every_put_with_409() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let (_, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": "before", "placement": "pre_roll" })),
    )
    .await;
    let ad_id = body["id"].as_str().unwrap().to_string();
    let path = format!("/ads/{ad_id}");
    let (status, _) = admin_call(&admin, reqwest::Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::OK);

    for put in [
        json!({ "status": "paused" }),
        json!({ "status": "ready" }),
        json!({ "title": "after" }),
        json!({ "placement": "mid_roll", "title": "after" }),
    ] {
        let (status, body) = admin_call(&admin, reqwest::Method::PUT, &path, Some(put.clone())).await;
        assert_eq!(status, StatusCode::CONFLICT, "{put}: {body}");
        assert_eq!((body["error"].as_str(), body["message"].as_str()), (Some("MM_AD_CONFLICT"), Some("This ad was deleted.")));
    }
    let (title, placement): (String, String) =
        sqlx::query_as("SELECT title, placement FROM mm_ad_creatives WHERE id = $1")
            .bind(&ad_id)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!((title.as_str(), placement.as_str()), ("before", "pre_roll"), "a refused PUT writes nothing");
    cleanup(&server.pool, &ad_id).await;
}

/// A PUT that answers with an error has changed nothing. A bad placement used to commit the pause
/// and then fail on the column's CHECK with a 500.
#[tokio::test]
async fn a_put_that_fails_changes_nothing() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let (_, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": "before", "placement": "mid_roll" })),
    )
    .await;
    let ad_id = body["id"].as_str().unwrap().to_string();
    let path = format!("/ads/{ad_id}");

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &path,
        Some(json!({ "status": "paused", "placement": "bogus", "title": "after" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("placement"), "{body}");
    let (st, placement, title): (String, String, String) =
        sqlx::query_as("SELECT status, placement, title FROM mm_ad_creatives WHERE id = $1")
            .bind(&ad_id)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!((st.as_str(), placement.as_str(), title.as_str()), ("draft", "mid_roll", "before"));

    // The same for a resume the ad cannot honour: the other fields are not applied either.
    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &path,
        Some(json!({ "status": "ready", "title": "after" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (st, title): (String, String) = sqlx::query_as("SELECT status, title FROM mm_ad_creatives WHERE id = $1")
        .bind(&ad_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!((st.as_str(), title.as_str()), ("draft", "before"));
    cleanup(&server.pool, &ad_id).await;
}

/// Lifting a pause from an ad that has no converted media is a success: it goes back to the state
/// its conversion is in, the response says which, and the rest of the PUT still applies.
#[tokio::test]
async fn resuming_a_paused_ad_without_media_reports_where_it_landed() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let (_, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": "T1", "placement": "pre_roll" })),
    )
    .await;
    let ad_id = body["id"].as_str().unwrap().to_string();
    let path = format!("/ads/{ad_id}");
    let (status, body) =
        admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "status": "paused" }))).await;
    assert_eq!((status, body["status"].as_str()), (StatusCode::OK, Some("paused")), "{body}");

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &path,
        Some(json!({ "status": "ready", "title": "T2" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["status"], "draft", "no media and no conversion: back to draft, not ready");
    let (st, title): (String, String) = sqlx::query_as("SELECT status, title FROM mm_ad_creatives WHERE id = $1")
        .bind(&ad_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!((st.as_str(), title.as_str()), ("draft", "T2"), "the DB agrees with the response");
    cleanup(&server.pool, &ad_id).await;
}

#[tokio::test]
async fn a_placement_that_is_not_a_slot_is_a_400_on_both_creates() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let owner = user("owner");
    let title = format!("bogus-{}", Uuid::new_v4().simple());

    let (status, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": title, "placement": "bogus" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("placement"), "{body}");

    let (status, body) = create_ad(&server, &owner, json!({ "title": title, "placement": "bogus" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap().contains("placement"), "{body}");

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_creatives WHERE title = $1")
        .bind(&title)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!((n, live_ads(&server, &owner).await), (0, 0), "a refused create leaves nothing");
}

#[tokio::test]
async fn a_converted_ad_lists_its_preview_and_cannot_move_over_a_slots_cap() {
    let Some(server) = start().await else { return };
    let _turn = PIPELINE.lock().await;
    let _dir = pipeline_dir(&server).await;
    let admin = admin_base(&server).await;
    let (_, body) = admin_call(
        &admin,
        reqwest::Method::POST,
        "/ads",
        Some(json!({ "title": "long", "placement": "mid_roll" })),
    )
    .await;
    let ad_id = body["id"].as_str().unwrap().to_string();
    let path = format!("/ads/{ad_id}");
    let (status, body) = upload(&format!("{admin}{path}/upload"), ADMIN_TOKEN, &mp4(1000)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let job = q::claim_next(&server.pool, "w").await.unwrap().expect("the upload queued a job");
    assert_eq!(job.creative_id, ad_id);
    let rel = format!("r/{ad_id}/x-webrtc_vp8-y.webm");
    let renditions = [NewRendition {
        kind: RenditionKind::WebrtcVp8, rel_path: rel.clone(), sha256: "e".repeat(64), bytes: 10,
        duration_ms: 45_000, width: 1280, height: 720, fps_num: 30, fps_den: 1, max_kf_gap_ms: 500,
        first_kf_bytes: 9000, integrated_lufs: None, has_source_audio: true,
    }];
    let published = Publication { renditions: &renditions, warnings: &[Warning::SilenceAdded], fetch_base: "http://mm-web/_mm/ads" };
    assert_eq!(q::publish(&server.pool, &job, "w", &published).await.unwrap(), PublishOutcome::Live);

    // The list carries the conversion: the preview a browser can play and the newest job.
    let (status, list) = admin_call(&admin, reqwest::Method::GET, "/ads", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let item = list["ads"].as_array().unwrap().iter().find(|a| a["id"] == ad_id.as_str()).expect("listed");
    assert_eq!(item["status"], "ready");
    assert_eq!(item["preview_url"], format!("/_mm/ads/{rel}"));
    assert_eq!(item["media"]["vp8"]["rel_path"], rel.as_str());
    assert_eq!(item["media"]["vp8"]["duration_ms"], 45_000);
    assert_eq!(item["media"]["job"]["state"], "succeeded");
    assert_eq!(item["media"]["job"]["warnings"], json!(["silence_added"]), "the newest job's, as before");
    assert_eq!(item["media"]["media_warnings"], json!(["silence_added"]), "and the live media's");

    // A cap lowered after the upload leaves this ad over mid-roll's new cap. Only a change of
    // placement is judged, so saving the stored placement back with another edit still works.
    set_config(&server, |c| c.advertising.mid_roll_max_secs = 30);
    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &path,
        Some(json!({ "placement": "mid_roll", "title": "echoed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) =
        admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "placement": "post_roll" }))).await;
    assert_eq!(status, StatusCode::OK, "post-roll's cap is the 60 s maximum: {body}");
    let (status, body) =
        admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "placement": "mid_roll" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "moving back to the lowered mid-roll cap is a change: {body}");
    set_config(&server, |c| c.advertising.mid_roll_max_secs = 60);
    let (status, body) =
        admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "placement": "mid_roll" }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 45 s fits mid-roll (60 s) but not pre-roll or any-slot (30 s): the row stays as it was.
    for (placement, label) in [("pre_roll", "pre-roll"), ("any", "any-slot")] {
        let (status, body) =
            admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "placement": placement }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{placement}: {body}");
        assert_eq!(body["message"], format!("This ad is 45 s; the {label} limit is 30 s."));
    }
    let placement: String = sqlx::query_scalar("SELECT placement FROM mm_ad_creatives WHERE id = $1")
        .bind(&ad_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(placement, "mid_roll");
    let (status, body) = admin_call(
        &admin,
        reqwest::Method::PUT,
        &path,
        Some(json!({ "placement": "post_roll", "title": "moved" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (placement, title): (String, String) =
        sqlx::query_as("SELECT placement, title FROM mm_ad_creatives WHERE id = $1")
            .bind(&ad_id)
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!((placement.as_str(), title.as_str()), ("post_roll", "moved"));

    // Pausing takes it off the air; resuming puts a converted ad straight back.
    for (wanted, expect) in [("paused", "paused"), ("ready", "ready")] {
        let (status, body) = admin_call(&admin, reqwest::Method::PUT, &path, Some(json!({ "status": wanted }))).await;
        assert_eq!(status, StatusCode::OK, "{wanted}: {body}");
        assert_eq!(creative_status(&server.pool, &ad_id).await, expect);
    }

    // Delete: the live rendition is retired, the source is stamped for removal, and it is idempotent.
    for _ in 0..2 {
        let (status, body) = admin_call(&admin, reqwest::Method::DELETE, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(creative_status(&server.pool, &ad_id).await, "deleted");
    let (live, purge): (i64, bool) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM mm_ad_renditions WHERE creative_id = $1 AND superseded_at IS NULL),
                (SELECT media_purge_after IS NOT NULL FROM mm_ad_creatives WHERE id = $1)",
    )
    .bind(&ad_id)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!((live, purge), (0, true));
    let (status, body) =
        admin_call(&admin, reqwest::Method::DELETE, &format!("/ads/{}", Uuid::new_v4()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (_, list) = admin_call(&admin, reqwest::Method::GET, "/ads", None).await;
    assert!(list["ads"].as_array().unwrap().iter().all(|a| a["id"] != ad_id.as_str()), "a deleted ad is not listed");
    cleanup(&server.pool, &ad_id).await;
}

/// The creator's delete is the same lifecycle delete, behind the owner check: the queued
/// conversion is cancelled, not left to run for an ad nobody can see.
#[tokio::test]
async fn deleting_a_creators_ad_cancels_its_queued_conversion() {
    let Some(server) = start().await else { return };
    let _turn = PIPELINE.lock().await;
    let _dir = pipeline_dir(&server).await;
    let owner = user("owner");
    let (status, body) = create_ad(&server, &owner, json!({ "title": "t", "placement": "pre_roll" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ad_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = upload(&format!("{}/ads/{ad_id}/upload", server.base), &bearer(&owner), &mp4(1000)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(creative_status(&server.pool, &ad_id).await, "processing");
    assert_eq!(job_states(&server.pool, &ad_id).await, ["queued"]);

    // Someone else's delete is a 404 and leaves the conversion alone.
    let (status, _) = delete_ad(&server, &user("other"), &ad_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(job_states(&server.pool, &ad_id).await, ["queued"]);

    let (status, body) = delete_ad(&server, &owner, &ad_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(creative_status(&server.pool, &ad_id).await, "deleted");
    assert_eq!(job_states(&server.pool, &ad_id).await, ["cancelled"]);
    assert!(q::claim_next(&server.pool, "w").await.unwrap().is_none(), "nothing is left to convert");
    cleanup(&server.pool, &ad_id).await;
}

// ── D8: only the creative's own file under mm-web is ever followed or handed out ─────────────

async fn set_cdn_url(pool: &PgPool, ad_id: &str, url: &str) {
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = $2 WHERE id = $1")
        .bind(ad_id)
        .bind(url)
        .execute(pool)
        .await
        .unwrap();
}

/// What the worker writes for a published rendition (`store::rel_path`).
fn rendition_url(ad_id: &str) -> String {
    format!("http://mm-web/_mm/ads/r/{ad_id}/01234567-webrtc_vp8-0123456789abcdef.webm")
}

/// `cdn_url` values a compromised worker could write (it holds `UPDATE (cdn_url)`), for `ad_id`.
fn hostile_urls(ad_id: &str, other: &str) -> Vec<String> {
    vec![
        "http://169.254.169.254/latest/meta-data/".into(),
        "https://cdn.example.org/ad.webm".into(),
        "http://mm-web.evil.example/_mm/ads/x.webm".into(),
        "http://mm-web:8080/_mm/ads/{ad}.webm".replace("{ad}", ad_id),
        format!("http://mm-web/_mm/ads/{ad_id}.webm?next=http://evil.example"),
        format!("http://mm-web/_mm/ads/r/{ad_id}/../../../etc/passwd"),
        format!("ftp://mm-web/_mm/ads/{ad_id}.webm"),
        rendition_url(other),
        format!("http://mm-web/_mm/ads/{other}.webm"),
        String::new(),
    ]
}

/// The decision engine skips a creative whose `cdn_url` is not its own file, logs it and serves no
/// ad for it: mm-switch would otherwise fetch whatever address the worker wrote.
#[tokio::test]
async fn the_decision_engine_serves_no_ad_for_a_cdn_url_that_is_not_its_own_file() {
    let Some(server) = start().await else { return };
    let host = user("host");
    let ad_id = creative(&server, &host).await;
    let other = Uuid::new_v4().to_string();
    // A fresh engine with platform ads off: other tests' platform creatives must not answer for it.
    let mut cfg = server.state.config().advertising.clone();
    cfg.platform_ads_enabled = false;
    let engine = mm_ads::AdDecisionEngine::new(cfg, server.pool.clone(), String::new());
    let decide = || async {
        let context = mm_ads::StreamAdContext {
            viewer_count: 10,
            stream_duration_secs: 600,
            categories: vec![],
            last_ad_at: None,
            host_user_id: host.clone(),
        };
        engine.decide(&format!("stream-{}", Uuid::new_v4().simple()), &user("viewer"), AdSlot::PreRoll, &context, true).await
    };

    for url in hostile_urls(&ad_id, &other) {
        set_cdn_url(&server.pool, &ad_id, &url).await;
        assert!(matches!(decide().await, mm_ads::AdDecision::NoAd { .. }), "served for {url:?}");
    }
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = NULL WHERE id = $1").bind(&ad_id).execute(&server.pool).await.unwrap();
    assert!(matches!(decide().await, mm_ads::AdDecision::NoAd { .. }), "no cdn_url, no address of the server's own to fall back to");

    for url in [rendition_url(&ad_id), format!("http://mm-web/_mm/ads/{ad_id}.webm")] {
        set_cdn_url(&server.pool, &ad_id, &url).await;
        match decide().await {
            mm_ads::AdDecision::ServeAd { ad, .. } => assert_eq!((ad.ad_id, ad.media_url), (ad_id.clone(), url)),
            other => panic!("not served: {other:?}"),
        }
    }

    // A creative with a bad URL does not shadow one with a good URL for the same slot.
    let bad = creative(&server, &host).await; // newer, so it is tried first
    for _ in 0..3 {
        match decide().await {
            mm_ads::AdDecision::ServeAd { ad, .. } => assert_eq!(ad.ad_id, ad_id),
            other => panic!("the good creative must still be served: {other:?}"),
        }
    }
    cleanup(&server.pool, &bad).await;
    cleanup(&server.pool, &ad_id).await;
}

/// `GET /ads/{id}/media` hands out the creative's own file and nothing else. It used to fall back
/// to an address built from the server's public URL when `cdn_url` was empty.
#[tokio::test]
async fn the_media_route_hands_out_only_the_creatives_own_file() {
    let Some(server) = start().await else { return };
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;
    let other = Uuid::new_v4().to_string();
    let media = |ad_id: String| {
        let server = &server;
        let owner = owner.clone();
        async move { call(server, reqwest::Method::GET, &owner, &format!("/ads/{ad_id}/media")).await }
    };

    for url in hostile_urls(&ad_id, &other) {
        set_cdn_url(&server.pool, &ad_id, &url).await;
        let (status, body) = media(ad_id.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{url:?}: {body}");
        assert!(body.get("url").is_none(), "{url:?}: {body}");
    }
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = NULL WHERE id = $1").bind(&ad_id).execute(&server.pool).await.unwrap();
    let (status, body) = media(ad_id.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    for url in [rendition_url(&ad_id), format!("http://mm-web/_mm/ads/{ad_id}.webm")] {
        set_cdn_url(&server.pool, &ad_id, &url).await;
        let (status, body) = media(ad_id.clone()).await;
        assert_eq!((status, body["url"].as_str()), (StatusCode::OK, Some(url.as_str())), "{body}");
    }
    let (status, _) = media(Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a creative that does not exist");
    cleanup(&server.pool, &ad_id).await;
}

// ── the worker's heartbeat, for the dashboard's "no worker" notice ───────────────────────────

/// Both admin reads carry `worker.heartbeat_age_secs`, the number behind the heartbeat gauge:
/// -1 while no worker has ever reported, then the seconds since the newest beat. The creator's own
/// conversion read does not carry operator information.
#[tokio::test]
async fn the_admin_reads_report_the_workers_heartbeat_age() {
    let Some(server) = start().await else { return };
    let admin = admin_base(&server).await;
    let owner = user("owner");
    let ad_id = creative(&server, &owner).await;
    let list_age = || async {
        let (status, body) = admin_call(&admin, reqwest::Method::GET, "/ads", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["ads"].is_array(), "the list itself is unchanged: {body}");
        body["worker"]["heartbeat_age_secs"].as_i64().unwrap_or_else(|| panic!("no worker.heartbeat_age_secs, only {:?}", body.as_object().map(|o| o.keys().collect::<Vec<_>>())))
    };
    let view_age = || async {
        let (status, body) = admin_call(&admin, reqwest::Method::GET, &format!("/ads/{ad_id}/conversion"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["creative_status"].is_string(), "the view itself is unchanged: {body}");
        body["worker"]["heartbeat_age_secs"].as_i64().unwrap_or_else(|| panic!("no worker.heartbeat_age_secs: {body}"))
    };

    sqlx::query("DELETE FROM mm_media_worker_status").execute(&server.pool).await.unwrap();
    assert_eq!((list_age().await, view_age().await), (-1, -1), "no worker has ever reported");

    q::worker_heartbeat(&server.pool, "wk-1", "test").await.unwrap();
    let (a, b) = (list_age().await, view_age().await);
    assert!((0..=3).contains(&a) && (0..=3).contains(&b), "just beat: {a} / {b}");

    // The newest of several workers counts, and the age grows.
    sqlx::query("UPDATE mm_media_worker_status SET heartbeat_at = now() - interval '300 seconds' WHERE worker_id = 'wk-1'")
        .execute(&server.pool).await.unwrap();
    let (a, b) = (list_age().await, view_age().await);
    assert!((300..=303).contains(&a) && (300..=303).contains(&b), "{a} / {b}");
    q::worker_heartbeat(&server.pool, "wk-2", "test").await.unwrap();
    assert!((0..=3).contains(&list_age().await));

    // The admin gate is unchanged, and the creator's read has no `worker`.
    let (status, _) = reqwest::Client::new()
        .get(format!("{admin}/ads"))
        .send()
        .await
        .map(|r| (r.status(), ()))
        .unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(&server, reqwest::Method::GET, &owner, &format!("/ads/{ad_id}/conversion")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("worker").is_none(), "operator information stays on the admin route: {body}");

    sqlx::query("DELETE FROM mm_media_worker_status").execute(&server.pool).await.unwrap();
    cleanup(&server.pool, &ad_id).await;
}
