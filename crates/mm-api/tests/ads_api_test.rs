//! Viewer ad events and the creator's creative read, on the real client router.
//!
//! `POST /ads/events`: a skip or a completion ends the ad. The handler drops the
//! scheduled switch-back, switches the viewer onto the programme and removes the ad's
//! source on mm-switch (here a stub that records what it was asked). The short `skip` /
//! `complete` forms used to be refused with 400 before that code ran.
//!
//! `GET /ads/{id}`: a creator reads their own creative and nobody else's. A creative
//! that is not yours answers exactly like one that does not exist.
//!
//! The router needs a full `AppState` against a real Postgres: the tests skip without
//! `MM_DATABASE_URL` and fail loudly with `MM_REQUIRE_DB` set, like the other DB tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
    state: SharedState,
    pool: PgPool,
    switch_calls: Calls,
}

/// An mm-switch that answers 200 to a switch and a source removal, and records both.
async fn stub_switch() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let router = Router::new()
        .route(
            "/api/switch",
            post(
                |State(calls): State<Calls>, axum::Json(body): axum::Json<Value>| async move {
                    calls.lock().unwrap().push(SwitchCall::Switch {
                        viewer: body["viewer_id"].as_str().unwrap_or_default().into(),
                        source: body["source_id"].as_str().unwrap_or_default().into(),
                    });
                    axum::Json(json!({ "ok": true }))
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
        admin_token: String::new(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    };
    let router = mm_api::client_router(state.clone()).layer(axum::Extension(auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Some(Server {
        base: format!("http://{addr}/_mm/client/v1"),
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
            started_at: std::time::Instant::now(),
            node_id: None,
            slot: AdSlot::PreRoll,
            billable: true,
        },
    );
    served
}

async fn report(server: &Server, served: &ServedAd, event: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/ads/events", server.base))
        .bearer_auth(bearer(&served.viewer))
        .json(&json!({ "impression_token": served.token, "event": event }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn get_ad(server: &Server, caller: &str, ad_id: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .get(format!("{}/ads/{ad_id}", server.base))
        .bearer_auth(bearer(caller))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
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
