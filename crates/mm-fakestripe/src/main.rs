//! mm-fakestripe — a minimal fake Stripe API server for MatrixMedia integration testing.
//!
//! Implements the exact subset of the Stripe REST API that mm-core's `async-stripe`
//! client calls. Does **not** attempt to be a general-purpose Stripe simulator.
//!
//! Endpoints:
//!   - `POST /v1/accounts`              → create an Express connected account
//!   - `POST /v1/account_links`         → create an onboarding link
//!   - `GET  /v1/accounts/{id}`         → retrieve an account
//!   - `POST /v1/checkout/sessions`     → create a checkout session and
//!                                        schedule a signed webhook back to mm-core
//!
//! On checkout creation, the fake fires a `checkout.session.completed` event back to
//! `MM_FAKESTRIPE_WEBHOOK_URL` after a short delay, signed with
//! `MM_FAKESTRIPE_WEBHOOK_SECRET` using the standard Stripe HMAC-SHA256 format.
//!
//! On account creation, the fake fires an `account.updated` event (with
//! `charges_enabled=true`) so creator onboarding flips to `complete`.
//!
//! Configuration:
//!   MM_FAKESTRIPE_LISTEN            e.g. "0.0.0.0:8787"
//!   MM_FAKESTRIPE_WEBHOOK_URL       e.g. "http://mm-core:6167/_mm/webhooks/stripe"
//!   MM_FAKESTRIPE_WEBHOOK_SECRET    the same whsec_* mm-core is configured with
//!   MM_FAKESTRIPE_WEBHOOK_DELAY_MS  default 500
//!
//! All fake IDs are `acct_fakeXXX`, `cs_fakeXXX`, `pi_fakeXXX`, `evt_fakeXXX`.

use axum::{
    Router,
    extract::{Form, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::{error, info, warn};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct AppState {
    webhook_url: String,
    webhook_secret: String,
    webhook_delay_ms: u64,
    http: reqwest::Client,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mm_fakestripe=info,tower_http=info".into()),
        )
        .init();

    let listen =
        std::env::var("MM_FAKESTRIPE_LISTEN").unwrap_or_else(|_| "0.0.0.0:8787".to_string());
    let webhook_url = std::env::var("MM_FAKESTRIPE_WEBHOOK_URL")
        .unwrap_or_else(|_| "http://mm-core:6167/_mm/webhooks/stripe".to_string());
    let webhook_secret = std::env::var("MM_FAKESTRIPE_WEBHOOK_SECRET").unwrap_or_default();
    let webhook_delay_ms: u64 = std::env::var("MM_FAKESTRIPE_WEBHOOK_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    if webhook_secret.is_empty() {
        warn!(
            "MM_FAKESTRIPE_WEBHOOK_SECRET is empty — outbound webhooks will be rejected by mm-core"
        );
    }

    let state = AppState {
        webhook_url: webhook_url.clone(),
        webhook_secret,
        webhook_delay_ms,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client"),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/v1/accounts", post(create_account))
        .route("/v1/accounts/{id}", get(retrieve_account))
        .route("/v1/account_links", post(create_account_link))
        .route("/v1/checkout/sessions", post(create_checkout_session))
        .route(
            "/_mm/cancel_subscription/{sub_id}",
            post(simulate_subscription_cancelled),
        )
        // -------- LNURL-pay (LUD-06 + LUD-16) mock --------
        // Lets the LNURL-pay client (mm-payment::lnurl) resolve a Lightning
        // Address like `alice@localhost:8787` against this server during
        // local demos / integration tests, with no real Lightning node.
        .route("/.well-known/lnurlp/{name}", get(fake_lnurl_metadata))
        .route("/_mm/fakeln/cb/{name}", get(fake_lnurl_callback))
        .with_state(Arc::new(state));

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .expect("bind MM_FAKESTRIPE_LISTEN");

    info!(
        listen = %listen,
        webhook_url = %webhook_url,
        webhook_delay_ms,
        "mm-fakestripe started"
    );

    axum::serve(listener, app).await.expect("serve");
}

async fn root() -> &'static str {
    "mm-fakestripe — Stripe API impersonator for MatrixMedia integration testing"
}

async fn healthz() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// POST /v1/accounts
// ---------------------------------------------------------------------------
//
// mm-payment::stripe::connect::create_connected_account calls this, then
// immediately creates an AccountLink, and returns both IDs to mm-core.
// After a short delay, we fire a signed `account.updated` webhook with
// `charges_enabled=true` so the creator's onboarding flag flips to complete.

async fn create_account(
    State(state): State<Arc<AppState>>,
    Form(params): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    // Capture metadata[*] pairs so we can echo them back in the webhook.
    let metadata = extract_metadata(&params);

    let acct_id = format!("acct_fake{}", short_id());
    let now = unix_now();

    let account_json = json!({
        "id": acct_id,
        "object": "account",
        "type": "express",
        "charges_enabled": true,
        "payouts_enabled": true,
        "details_submitted": true,
        "country": "US",
        "default_currency": "usd",
        "created": now,
        "deleted": false,
        "email": null,
        "metadata": metadata,
    });

    info!(acct_id = %acct_id, "created fake account");

    // Fire account.updated webhook after delay. This causes mm-core to flip
    // the creator's `onboarding_complete` flag when `charges_enabled=true`.
    let state2 = state.clone();
    let acct_for_webhook = account_json.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(state2.webhook_delay_ms)).await;
        let event = build_event("account.updated", acct_for_webhook);
        if let Err(e) = send_signed_webhook(&state2, &event).await {
            error!(error = %e, "failed to send account.updated webhook");
        }
    });

    (StatusCode::OK, axum::Json(account_json))
}

// ---------------------------------------------------------------------------
// GET /v1/accounts/{id}
// ---------------------------------------------------------------------------
//
// mm-payment::stripe::connect::check_onboarding_status reads `charges_enabled`.
// Return the same shape as POST /v1/accounts with charges_enabled=true.

async fn retrieve_account(
    State(_state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let now = unix_now();
    let account_json = json!({
        "id": id,
        "object": "account",
        "type": "express",
        "charges_enabled": true,
        "payouts_enabled": true,
        "details_submitted": true,
        "country": "US",
        "default_currency": "usd",
        "created": now,
        "deleted": false,
        "email": null,
        "metadata": {},
    });
    (StatusCode::OK, axum::Json(account_json))
}

// ---------------------------------------------------------------------------
// POST /v1/account_links
// ---------------------------------------------------------------------------

async fn create_account_link(
    State(_state): State<Arc<AppState>>,
    Form(params): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let acct_id = params
        .get("account")
        .cloned()
        .unwrap_or_else(|| "acct_fake_unknown".to_string());
    let now = unix_now();

    let link = json!({
        "object": "account_link",
        "url": format!("https://fake-stripe.local/onboarding/{acct_id}?ts={now}"),
        "created": now,
        "expires_at": now + 300,
    });

    info!(acct_id = %acct_id, "created fake account link");

    (StatusCode::OK, axum::Json(link))
}

// ---------------------------------------------------------------------------
// POST /v1/checkout/sessions
// ---------------------------------------------------------------------------
//
// The critical endpoint. mm-payment sends price_data, metadata, transfer_data.
// We generate a stable session_id, return it, and schedule a signed
// `checkout.session.completed` webhook referencing that same session_id.
//
// mm-core's webhook handler looks up the donation row by stripe_session_id
// and flips status from pending → succeeded.

async fn create_checkout_session(
    State(state): State<Arc<AppState>>,
    Form(params): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let metadata = extract_metadata(&params);
    let mode = params
        .get("mode")
        .cloned()
        .unwrap_or_else(|| "payment".to_string());
    let success_url = params.get("success_url").cloned();
    let cancel_url = params.get("cancel_url").cloned();

    let is_subscription = mode == "subscription";
    let session_id = format!("cs_fake{}", short_id());
    let now = unix_now();

    // Payment sessions have a payment_intent; subscription sessions have a
    // subscription id. Populate whichever one is relevant so mm-core's webhook
    // handler can dispatch correctly.
    let payment_intent_id: Option<String> = if is_subscription {
        None
    } else {
        Some(format!("pi_fake{}", short_id()))
    };
    let subscription_id: Option<String> = if is_subscription {
        Some(format!("sub_fake{}", short_id()))
    } else {
        None
    };

    // The session returned to mm-core (status=open, unpaid — as if the customer
    // has not yet paid). We'll mutate it for the webhook below.
    let session_response = json!({
        "id": session_id,
        "object": "checkout.session",
        "mode": mode,
        "url": format!("https://fake-stripe.local/checkout/{session_id}"),
        "status": "open",
        "payment_status": "unpaid",
        "created": now,
        "expires_at": now + 1800,
        "livemode": false,
        "payment_method_types": ["card"],
        "currency": "usd",
        "metadata": metadata.clone(),
        "success_url": success_url,
        "cancel_url": cancel_url,
        "automatic_tax": { "enabled": false, "liability": null, "status": null },
        "custom_fields": [],
        "custom_text": {
            "after_submit": null,
            "shipping_address": null,
            "submit": null,
            "terms_of_service_acceptance": null
        },
        "shipping_options": [],
    });

    info!(
        session_id = %session_id,
        mode = %mode,
        subscription_id = ?subscription_id,
        "created fake checkout session"
    );

    // Spawn the webhook AFTER the response is sent.
    let state2 = state.clone();
    let session_for_webhook = json!({
        "id": session_id,
        "object": "checkout.session",
        "mode": mode,
        "url": format!("https://fake-stripe.local/checkout/{session_id}"),
        "status": "complete",
        "payment_status": "paid",
        "payment_intent": payment_intent_id,
        "subscription": subscription_id,
        "created": now,
        "expires_at": now + 1800,
        "livemode": false,
        "payment_method_types": ["card"],
        "currency": "usd",
        "metadata": metadata,
        "success_url": success_url,
        "cancel_url": cancel_url,
        "automatic_tax": { "enabled": false, "liability": null, "status": null },
        "custom_fields": [],
        "custom_text": {
            "after_submit": null,
            "shipping_address": null,
            "submit": null,
            "terms_of_service_acceptance": null
        },
        "shipping_options": [],
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(state2.webhook_delay_ms)).await;
        let event = build_event("checkout.session.completed", session_for_webhook);
        if let Err(e) = send_signed_webhook(&state2, &event).await {
            error!(error = %e, "failed to send checkout.session.completed webhook");
        }
    });

    (StatusCode::OK, axum::Json(session_response))
}

// ---------------------------------------------------------------------------
// POST /v1/_mm/cancel_subscription/{sub_id}
// ---------------------------------------------------------------------------
//
// This is NOT a real Stripe endpoint — it's an admin hook the test harness
// uses to trigger a `customer.subscription.deleted` webhook without touching
// mm-core's cancel endpoint. Useful for testing the webhook-driven cancel
// path end-to-end.

async fn simulate_subscription_cancelled(
    State(state): State<Arc<AppState>>,
    Path(sub_id): Path<String>,
) -> impl IntoResponse {
    let now = unix_now();
    // async-stripe's Subscription type has many non-Option required fields.
    // The set below matches the minimum surface that deserializes cleanly.
    let sub_object = json!({
        "id": sub_id,
        "object": "subscription",
        "automatic_tax": { "enabled": false, "liability": null, "status": null },
        "billing_cycle_anchor": now - 3600,
        "cancel_at_period_end": false,
        "canceled_at": now,
        "created": now - 3600,
        "currency": "usd",
        "current_period_start": now - 3600,
        "current_period_end": now + 86400,
        "customer": format!("cus_fake{}", short_id()),
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
    });
    info!(sub_id = %sub_id, "simulating customer.subscription.deleted webhook");
    let state2 = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(state2.webhook_delay_ms)).await;
        let event = build_event("customer.subscription.deleted", sub_object);
        if let Err(e) = send_signed_webhook(&state2, &event).await {
            error!(error = %e, "failed to send customer.subscription.deleted webhook");
        }
    });
    StatusCode::ACCEPTED
}

// ---------------------------------------------------------------------------
// Webhook plumbing
// ---------------------------------------------------------------------------

fn build_event(event_type: &str, object: Value) -> Value {
    json!({
        "id": format!("evt_fake{}", short_id()),
        "object": "event",
        "api_version": "2024-11-20.acacia",
        "created": unix_now(),
        "data": {
            "object": object,
        },
        "livemode": false,
        "pending_webhooks": 0,
        "request": { "id": null, "idempotency_key": null },
        "type": event_type,
    })
}

async fn send_signed_webhook(
    state: &AppState,
    event: &Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let payload = serde_json::to_string(event)?;
    let ts = unix_now();
    let signed_payload = format!("{}.{}", ts, payload);

    let mut mac = HmacSha256::new_from_slice(state.webhook_secret.as_bytes())
        .map_err(|e| format!("HMAC key error: {e}"))?;
    mac.update(signed_payload.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    let header = format!("t={ts},v1={sig}");

    info!(
        url = %state.webhook_url,
        event_type = %event.get("type").and_then(|v| v.as_str()).unwrap_or("?"),
        event_id = %event.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
        "sending signed webhook"
    );

    let res = state
        .http
        .post(&state.webhook_url)
        .header("Content-Type", "application/json")
        .header("Stripe-Signature", header)
        .body(payload)
        .send()
        .await?;

    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status.is_success() {
        info!(status = %status, "webhook delivered");
    } else {
        warn!(status = %status, body = %body, "webhook delivery non-2xx");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// LNURL-pay (LUD-06 + LUD-16) mock
// ---------------------------------------------------------------------------
//
// Lets local demos exercise the true-P2P Lightning path without a real
// Lightning Address. Configure a creator's lightning_address as
// `<name>@localhost:8787` (or whatever MM_FAKESTRIPE_LISTEN binds to).
//
// `mm-payment::lnurl::LnurlPayClient` will then resolve via the LUD-16
// well-known URL pattern, which now uses HTTP for `.localhost`/`.local`/
// `.test` domains and bare-loopback addresses.
//
// The "callback" returns a syntactically-correct fake BOLT11 invoice the
// donor wallet would normally pay. There is no real settlement — for
// integration tests, mm-core treats it as `succeeded` immediately because
// the donation row's session_id is the donation_id itself (LNURL-pay path
// has no operator-side webhook by design).

#[derive(serde::Deserialize)]
struct CallbackQuery {
    amount: u64,
    #[serde(default)]
    comment: Option<String>,
}

async fn fake_lnurl_metadata(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let _ = state; // unused — purely informational endpoint
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8787")
        .to_string();
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    let _ = uri;

    let callback = format!("{scheme}://{host}/_mm/fakeln/cb/{name}");
    let metadata_array = format!(
        r#"[["text/plain","Sats for {name}"],["text/identifier","{name}@{host}"]]"#
    );

    let body = json!({
        "callback": callback,
        // 0.0001 BTC max, 1 sat min — wide enough for any demo amount.
        "maxSendable": 10_000_000_000u64,
        "minSendable": 1_000u64,
        "metadata": metadata_array,
        "tag": "payRequest",
        "commentAllowed": 200u32,
    });

    info!(name = %name, "fakeln: returned LUD-06 metadata");
    (StatusCode::OK, axum::Json(body))
}

async fn fake_lnurl_callback(
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> impl IntoResponse {
    // Synthesise a believable-looking BOLT11. Real wallets would reject
    // this (no valid signature / preimage) but mm-core's LNURL-pay path
    // does not parse it — it just hands the string to the donor client,
    // which in fakeln integration tests never actually pays it.
    let pr = format!(
        "lnbc{}n1pfake{}{}",
        // amount in nanosats — close enough for the textarea
        q.amount.max(1_000) / 1_000,
        short_id(),
        short_id()
    );
    let body = json!({
        "pr": pr,
        "routes": [],
        "successAction": {
            "tag": "message",
            "message": format!("Thanks for the sats, {name}!")
        }
    });
    info!(name = %name, amount_msat = q.amount, comment = ?q.comment, "fakeln: returned BOLT11 stub");
    (StatusCode::OK, axum::Json(body))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Stripe form encoding uses `metadata[key]=value` pairs. Lift those into a
/// JSON object.
fn extract_metadata(params: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in params {
        if let Some(inner) = k.strip_prefix("metadata[")
            && let Some(key) = inner.strip_suffix(']')
        {
            out.insert(key.to_string(), v.clone());
        }
    }
    out
}
