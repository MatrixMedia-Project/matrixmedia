//! The tier gate's permissions cache must not outlive a subscription change.
//!
//! `tier_gate::effective_permissions` caches `(subscriber, room) → TierPermissions` for
//! 60 s. A path that turns a subscription on or off without dropping the affected entries
//! leaves the gate answering from the old state until the TTL runs out: a viewer who just
//! paid keeps getting 403s and withheld recording URLs, and a cancelled one keeps access.
//!
//! Each test primes the cache through the real gate, changes the subscription through the
//! real handler (a signed Stripe webhook, or the subscriber's own
//! `DELETE /subscriptions/{id}`), and reads the gate again straight away. The cache key is
//! the Matrix room id, so a creator-wide subscription (`room_id` NULL), which the resolver
//! applies to every room of that creator, must refresh each of those rooms.
//!
//! The router needs a full `AppState` against a real Postgres: the tests skip without
//! `MM_DATABASE_URL` and fail loudly with `MM_REQUIRE_DB` set, like the other DB tests.

use std::collections::HashMap;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mm_api::middleware::{AuthConfig, tier_gate};
use mm_api::settings_service::{BootOptions, SettingsService};
use mm_api::state::{AppState, SharedState};
use mm_core::cache::TokenCache;
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::metrics::Metrics;
use mm_core::permissions::TierPermissions;
use mm_db::PgDatabase;
use mm_db::test_support::require_or_try_pool;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_sfu::CircuitBreakerAdapter;
use mm_sfu::livekit::LiveKitAdapter;

const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";
const WEBHOOK_SECRET: &str = "whsec_test_0123456789abcdef0123456789abcdef";
/// Nothing listens on the discard port, so no outbound call reaches a real service.
const DEAD: &str = "http://127.0.0.1:9";

struct Server {
    base: String,
    state: SharedState,
    pool: PgPool,
}

async fn start() -> Option<Server> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    let database_url = std::env::var("MM_DATABASE_URL").expect("require_or_try_pool saw it");
    let db = PgDatabase::new(&database_url).await.expect("database");

    let mut config = Config::default();
    config.matrix.homeserver_url = DEAD.into();
    config.sfu.livekit_url = Some(DEAD.into());
    // The settings service imports its base config into the shared database and overlays
    // whatever rows are already there, so the first test binary to boot decides every
    // later one's config. Boot it with the plain config the other AppState tests use (so
    // nothing monetized is imported for them), and give the state its own handle, so a
    // stored `monetization.enabled = false` cannot switch these routes off.
    let settings = SettingsService::boot(
        db.pool().clone(),
        config.clone(),
        None,
        BootOptions::for_tests(),
        CancellationToken::new(),
    )
    .await
    .expect("settings boot");
    config.monetization.enabled = true;
    config.monetization.subscriptions_enabled = true;
    config.monetization.webhook_signing_secret = WEBHOOK_SECRET.into();

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
        ad_engine: None,
        switch_pool: None,
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
        // Built as startup.rs builds it: the 60 s TTL is what makes a stale entry visible.
        permissions_cache: Arc::new(tier_gate::new_permissions_cache()),
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
        base: format!("http://{addr}"),
        state,
        pool,
    })
}

/// Fresh ids per test, so tests sharing the database never see each other's rows.
struct Fixture {
    creator: String,
    subscriber: String,
    rooms: [String; 2],
}

fn fixture(tag: &str) -> Fixture {
    let n = Uuid::new_v4().simple().to_string();
    Fixture {
        creator: format!("@pc_creator_{tag}_{n}:s"),
        subscriber: format!("@pc_viewer_{tag}_{n}:s"),
        rooms: [format!("!pc_{tag}_a_{n}:s"), format!("!pc_{tag}_b_{n}:s")],
    }
}

async fn cleanup(pool: &PgPool, f: &Fixture) {
    sqlx::query("DELETE FROM mm_subscriptions WHERE creator_user_id = $1")
        .bind(&f.creator)
        .execute(pool)
        .await
        .expect("cleanup subs");
    sqlx::query("DELETE FROM mm_subscription_tiers WHERE creator_user_id = $1")
        .bind(&f.creator)
        .execute(pool)
        .await
        .expect("cleanup tiers");
}

/// A paid tier whose permissions include `can_watch_recordings` and `can_join_live`,
/// scoped to `room` (or creator-wide when `None`).
async fn paid_tier(pool: &PgPool, creator: &str, room: Option<&str>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO mm_subscription_tiers
            (creator_user_id, room_id, tier_level, name, price_cents, currency,
             perks_json, permissions, is_active)
         VALUES ($1, $2, 1, 'Supporter', 500, 'usd', '[]'::jsonb, $3, true)
         RETURNING id",
    )
    .bind(creator)
    .bind(room)
    .bind(serde_json::to_value(TierPermissions::full_size_user_default()).unwrap())
    .fetch_one(pool)
    .await
    .expect("create paid tier")
}

/// A subscription row in `status`, keyed to Stripe by `stripe_id`.
async fn subscription(
    pool: &PgPool,
    f: &Fixture,
    room: Option<&str>,
    tier_id: Uuid,
    status: &str,
    stripe_id: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO mm_subscriptions
            (subscriber_user_id, creator_user_id, room_id, tier_id, status,
             stripe_subscription_id, current_period_end)
         VALUES ($1, $2, $3, $4, $5, $6, now() + interval '30 days')
         RETURNING id",
    )
    .bind(&f.subscriber)
    .bind(&f.creator)
    .bind(room)
    .bind(tier_id)
    .bind(status)
    .bind(stripe_id)
    .fetch_one(pool)
    .await
    .expect("create subscription")
}

/// The gate's answer for `(subscriber, room)`, through the cache, as handlers read it.
async fn perms(s: &Server, f: &Fixture, room: &str) -> TierPermissions {
    tier_gate::effective_permissions(&s.state, &f.subscriber, &f.creator, room)
        .await
        .ok()
        .expect("effective_permissions")
}

/// POST a Stripe event to the webhook, signed the way Stripe signs it.
async fn send_webhook(s: &Server, event_type: &str, object: Value) {
    let now = chrono::Utc::now().timestamp();
    let payload = json!({
        "id": format!("evt_test_{}", Uuid::new_v4().simple()),
        "object": "event",
        "api_version": "2024-11-20.acacia",
        "created": now,
        "data": { "object": object },
        "livemode": false,
        "pending_webhooks": 0,
        "request": { "id": null, "idempotency_key": null },
        "type": event_type,
    })
    .to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(format!("{now}.{payload}").as_bytes());
    let signature = format!("t={now},v1={}", hex::encode(mac.finalize().into_bytes()));

    let res = reqwest::Client::new()
        .post(format!("{}/_mm/webhooks/stripe", s.base))
        .header("Content-Type", "application/json")
        .header("Stripe-Signature", signature)
        .body(payload)
        .send()
        .await
        .expect("webhook request");
    assert_eq!(res.status(), StatusCode::OK, "webhook accepted");
}

/// The `checkout.session.completed` Stripe sends when a subscription checkout is paid
/// (same shape mm-fakestripe emits).
fn checkout_completed(session_id: &str, stripe_sub_id: &str) -> Value {
    let now = chrono::Utc::now().timestamp();
    json!({
        "id": session_id,
        "object": "checkout.session",
        "mode": "subscription",
        "url": null,
        "status": "complete",
        "payment_status": "paid",
        "payment_intent": null,
        "subscription": stripe_sub_id,
        "created": now,
        "expires_at": now + 1800,
        "livemode": false,
        "payment_method_types": ["card"],
        "currency": "usd",
        "metadata": {},
        "success_url": "https://example.org/ok",
        "cancel_url": "https://example.org/cancel",
        "automatic_tax": { "enabled": false, "liability": null, "status": null },
        "custom_fields": [],
        "custom_text": {
            "after_submit": null,
            "shipping_address": null,
            "submit": null,
            "terms_of_service_acceptance": null
        },
        "shipping_options": [],
    })
}

/// The `customer.subscription.deleted` object (the fields async-stripe requires).
fn subscription_deleted(stripe_sub_id: &str) -> Value {
    let now = chrono::Utc::now().timestamp();
    json!({
        "id": stripe_sub_id,
        "object": "subscription",
        "automatic_tax": { "enabled": false, "liability": null, "status": null },
        "billing_cycle_anchor": now - 3600,
        "cancel_at_period_end": false,
        "canceled_at": now,
        "created": now - 3600,
        "currency": "usd",
        "current_period_start": now - 3600,
        "current_period_end": now + 86400,
        "customer": "cus_test",
        "items": {
            "object": "list",
            "data": [],
            "has_more": false,
            "url": "/v1/subscription_items"
        },
        "livemode": false,
        "metadata": {},
        "start_date": now - 3600,
        "status": "canceled",
    })
}

fn assert_paid(p: &TierPermissions, when: &str) {
    assert!(p.can_watch_recordings, "{when}: can_watch_recordings");
    assert!(p.can_join_live, "{when}: can_join_live");
}

fn assert_unpaid(p: &TierPermissions, when: &str) {
    assert!(!p.can_watch_recordings, "{when}: can_watch_recordings");
    assert!(!p.can_join_live, "{when}: can_join_live");
}

#[tokio::test]
async fn activation_webhook_unlocks_a_room_scoped_subscription_at_once() {
    let Some(s) = start().await else { return };
    let f = fixture("act_room");
    let room = f.rooms[0].as_str();
    let tier = paid_tier(&s.pool, &f.creator, Some(room)).await;
    let session = format!("cs_test_{}", Uuid::new_v4().simple());
    subscription(&s.pool, &f, Some(room), tier, "incomplete", &session).await;

    assert_unpaid(&perms(&s, &f, room).await, "before checkout");

    let stripe_sub = format!("sub_test_{}", Uuid::new_v4().simple());
    send_webhook(
        &s,
        "checkout.session.completed",
        checkout_completed(&session, &stripe_sub),
    )
    .await;

    assert_paid(&perms(&s, &f, room).await, "right after activation");
    cleanup(&s.pool, &f).await;
}

#[tokio::test]
async fn activation_webhook_unlocks_every_room_of_a_creator_wide_subscription() {
    let Some(s) = start().await else { return };
    let f = fixture("act_wide");
    let tier = paid_tier(&s.pool, &f.creator, None).await;
    let session = format!("cs_test_{}", Uuid::new_v4().simple());
    subscription(&s.pool, &f, None, tier, "incomplete", &session).await;

    for room in &f.rooms {
        assert_unpaid(&perms(&s, &f, room).await, "before checkout");
    }

    let stripe_sub = format!("sub_test_{}", Uuid::new_v4().simple());
    send_webhook(
        &s,
        "checkout.session.completed",
        checkout_completed(&session, &stripe_sub),
    )
    .await;

    for room in &f.rooms {
        assert_paid(
            &perms(&s, &f, room).await,
            &format!("{room} right after activation"),
        );
    }
    cleanup(&s.pool, &f).await;
}

#[tokio::test]
async fn stripe_cancel_webhook_revokes_a_room_scoped_subscription_at_once() {
    let Some(s) = start().await else { return };
    let f = fixture("del_room");
    let room = f.rooms[0].as_str();
    let tier = paid_tier(&s.pool, &f.creator, Some(room)).await;
    let stripe_sub = format!("sub_test_{}", Uuid::new_v4().simple());
    subscription(&s.pool, &f, Some(room), tier, "active", &stripe_sub).await;

    assert_paid(&perms(&s, &f, room).await, "while subscribed");

    send_webhook(
        &s,
        "customer.subscription.deleted",
        subscription_deleted(&stripe_sub),
    )
    .await;

    assert_unpaid(&perms(&s, &f, room).await, "right after Stripe cancelled");
    cleanup(&s.pool, &f).await;
}

#[tokio::test]
async fn subscriber_cancel_revokes_every_room_of_a_creator_wide_subscription() {
    let Some(s) = start().await else { return };
    let f = fixture("cancel_wide");
    let tier = paid_tier(&s.pool, &f.creator, None).await;
    // Not a `sub_*` id, so the handler skips the Stripe API and only updates the row.
    let sub_id = subscription(&s.pool, &f, None, tier, "active", "cs_test_settled").await;

    for room in &f.rooms {
        assert_paid(&perms(&s, &f, room).await, "while subscribed");
    }

    let (token, _) = mm_core::auth::issue_session_token(&f.subscriber, JWT_KEY).unwrap();
    let res = reqwest::Client::new()
        .delete(format!("{}/_mm/client/v1/subscriptions/{sub_id}", s.base))
        .bearer_auth(token)
        .send()
        .await
        .expect("cancel request");
    assert_eq!(res.status(), StatusCode::NO_CONTENT, "cancel accepted");

    for room in &f.rooms {
        assert_unpaid(
            &perms(&s, &f, room).await,
            &format!("{room} right after cancel"),
        );
    }
    cleanup(&s.pool, &f).await;
}
