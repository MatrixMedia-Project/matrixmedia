//! Monetization API handlers (Phase 7a: Donations, Phase 7b: Subscriptions).
//!
//! All endpoints check `state.config.monetization.enabled` and return
//! 501 MM_MONETIZATION_DISABLED when the feature is off.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::guards::{
    db, entitlement_service, payment_registry, pg_pool, require_donations, require_monetization,
    require_subscriptions,
};
use crate::middleware::AuthUser;
use crate::state::SharedState;
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::StreamId;
use mm_db::Database;
use mm_db::models::{Donation, DonationStatus};
use mm_payment::donations::{calculate_fees, tier_for_amount};
use mm_payment::provider::{CheckoutMode, CheckoutRequest, OnboardingRequest};
use mm_payment::subscriptions;

// ---------------------------------------------------------------------------
// POST /creator/onboard
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatorOnboardRequest {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct CreatorOnboardResponse {
    pub creator_id: Uuid,
    pub onboarding_url: String,
}

/// Start Stripe Connect onboarding for the authenticated user.
///
/// Creates a Stripe Express connected account, saves the stripe_account_id to
/// mm_creator_profiles, and returns the Stripe-hosted onboarding URL.
pub async fn creator_onboard(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreatorOnboardRequest>,
) -> Result<Json<CreatorOnboardResponse>, ApiError> {
    require_monetization(&state)?;
    let db = db(&state);
    let registry = payment_registry(&state)?;

    let user_id = auth.user_id.0.as_str();

    let base_url = state
        .config
        .server
        .public_url
        .as_deref()
        .unwrap_or("https://10.0.0.105:6167");

    // Check if profile already exists with a stripe account (idempotent).
    if let Some(existing) = db.get_creator_profile(user_id).await?
        && existing.stripe_account_id.is_some()
    {
        // Already onboarded or in progress -- create a fresh onboarding link.
        let onboard_resp = registry
            .onboard(
                "stripe",
                OnboardingRequest {
                    user_id: user_id.to_string(),
                    return_url: format!("{base_url}/creator/onboard/return"),
                    refresh_url: format!("{base_url}/creator/onboard/refresh"),
                },
            )
            .await
            .map_err(|e| MMError::Stripe(e.to_string()))?;

        return Ok(Json(CreatorOnboardResponse {
            creator_id: existing.id,
            onboarding_url: onboard_resp.onboarding_url,
        }));
    }

    // Create or upsert creator profile in PG.
    let profile = db
        .create_creator_profile(
            user_id,
            &req.display_name,
            state.config.monetization.platform_fee_pct,
        )
        .await?;

    // Create Stripe Express connected account via payment registry.
    let base_url = state
        .config
        .server
        .public_url
        .as_deref()
        .unwrap_or("https://localhost:6167");
    let registry = payment_registry(&state)?;

    let onboard_resp = registry
        .onboard(
            "stripe",
            OnboardingRequest {
                user_id: user_id.to_string(),
                return_url: format!("{base_url}/creator/onboard/return"),
                refresh_url: format!("{base_url}/creator/onboard/refresh"),
            },
        )
        .await
        .map_err(|e| MMError::Stripe(e.to_string()))?;

    // Save stripe_account_id to the creator profile.
    db.set_creator_stripe_account(user_id, &onboard_resp.account_id)
        .await?;

    state.metrics.creator_onboarding_total.inc();

    Ok(Json(CreatorOnboardResponse {
        creator_id: profile.id,
        onboarding_url: onboard_resp.onboarding_url,
    }))
}

// ---------------------------------------------------------------------------
// GET /creator/profile
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CreatorProfileResponse {
    pub id: Uuid,
    pub user_id: String,
    pub display_name: String,
    pub onboarding_complete: bool,
    pub platform_fee_pct: f64,
    pub created_at: String,
}

/// Return the creator profile for the authenticated user.
pub async fn get_creator_profile(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<CreatorProfileResponse>, ApiError> {
    require_monetization(&state)?;
    let db = db(&state);

    let profile = db
        .get_creator_profile(auth.user_id.0.as_str())
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Creator profile not found"))?;

    Ok(Json(CreatorProfileResponse {
        id: profile.id,
        user_id: profile.user_id,
        display_name: profile.display_name,
        onboarding_complete: profile.onboarding_complete,
        platform_fee_pct: profile.platform_fee_pct,
        created_at: profile.created_at.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// POST /donations
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateDonationRequest {
    pub stream_id: String,
    pub amount_cents: i64,
    pub message: Option<String>,
    /// Payment provider: "stripe" (default) or "lightning".
    /// Default keeps existing clients working unchanged.
    #[serde(default = "default_payment_provider")]
    pub payment_provider: String,
}

fn default_payment_provider() -> String {
    "stripe".to_owned()
}

#[derive(Debug, Serialize)]
pub struct CreateDonationResponse {
    pub donation_id: Uuid,
    /// For Stripe: `https://checkout.stripe.com/...`. For Lightning: BOLT11 string.
    pub checkout_url: String,
    /// Lightning-only metadata. Omitted for Stripe responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invoice: Option<LightningInvoice>,
    pub tier: String,
    pub pin_duration_secs: u32,
}

/// Lightning invoice metadata returned for `payment_provider == "lightning"`.
/// Sufficient for the client to render a QR + poll payment status.
#[derive(Debug, Serialize)]
pub struct LightningInvoice {
    /// BOLT11 invoice string (e.g. `lnbc500m1...`).
    pub bolt11: String,
    /// Payment hash (hex), used for status polling.
    pub payment_hash: String,
    /// QR code as `data:image/svg+xml;base64,...`. Populated by follow-up PR (PR B).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr_data_url: Option<String>,
}

/// Create a donation to the host of the specified stream.
///
/// Returns a Stripe Checkout URL. The viewer is redirected there to complete payment.
pub async fn create_donation(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreateDonationRequest>,
) -> Result<Json<CreateDonationResponse>, ApiError> {
    require_donations(&state)?;
    let db = db(&state);

    // M13: Validate donation amount (positive + within configured bounds)
    mm_core::validation::validate_donation_amount(
        req.amount_cents,
        state.config.monetization.min_donation_cents,
        state.config.monetization.max_donation_cents,
    )?;

    // M7: Sanitize donation message (strip control chars, HTML-escape, truncate)
    let message = req
        .message
        .map(|m| mm_core::validation::sanitize_display_text(&m, 150));

    // Look up the stream to get the host user_id.
    let stream = state
        .db
        .get_stream(&StreamId(req.stream_id.clone()))
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Stream not found"))?;

    if stream.status != "active" {
        return Err(MMError::api(ErrorCode::StreamEnded, "Stream is not active").into());
    }

    // Look up the creator profile for the stream host.
    let creator = db
        .get_creator_profile(&stream.host_user_id)
        .await?
        .ok_or_else(|| {
            MMError::api(
                ErrorCode::CreatorNotOnboarded,
                "Stream host has not set up donations",
            )
        })?;

    if !creator.onboarding_complete {
        return Err(MMError::api(
            ErrorCode::CreatorNotOnboarded,
            "Stream host has not completed Stripe onboarding",
        )
        .into());
    }

    let stripe_account_id = creator.stripe_account_id.as_deref().ok_or_else(|| {
        MMError::api(
            ErrorCode::CreatorNotOnboarded,
            "Stream host has no Stripe account",
        )
    })?;

    // Calculate tier and fees.
    let tier_info = tier_for_amount(req.amount_cents);
    let fees = calculate_fees(req.amount_cents, creator.platform_fee_pct);

    // Validate + normalize the requested payment provider. Lightning aliases
    // accepted for ergonomics. Anything else → 400 with a stable error code so
    // the client can surface a useful message.
    let provider = match req.payment_provider.to_lowercase().as_str() {
        "lightning" | "ln" => "lightning",
        "stripe" | "" => "stripe",
        other => {
            return Err(MMError::api(
                ErrorCode::InvalidPaymentProvider,
                format!("Unknown payment provider: {other}"),
            )
            .into());
        }
    };

    // Lightning is denominated in sats. Convert at our pinned 1500 sats/USD
    // (see `mm-payment::lnbits::types`); a live oracle is M3 work.
    let amount_for_provider = if provider == "lightning" {
        mm_payment::lnbits::types::usd_cents_to_sats(req.amount_cents)
    } else {
        req.amount_cents
    };
    let currency_for_provider = if provider == "lightning" { "sats" } else { "usd" };

    // Generate donation ID and idempotency key.
    let donation_id = Uuid::new_v4();
    let idempotency_key = Uuid::new_v4().to_string();

    // Build metadata for the checkout session (Stripe + Lightning both honor it).
    let donor_user_id = auth.user_id.0.clone();
    let mut metadata = std::collections::HashMap::with_capacity(3);
    metadata.insert("donation_id".to_owned(), donation_id.to_string());
    metadata.insert("stream_id".to_owned(), req.stream_id.clone());
    metadata.insert("donor_user_id".to_owned(), donor_user_id.clone());

    // Resolve checkout via the payment-provider registry. The provider name is
    // dispatched dynamically so we can support more rails (Phase M6 adapters)
    // without touching this handler.
    let base_url = state
        .config
        .server
        .public_url
        .as_deref()
        .unwrap_or("https://localhost:6167");
    let registry = payment_registry(&state)?;

    let checkout_resp = registry
        .create_checkout(
            provider,
            CheckoutRequest {
                mode: CheckoutMode::Payment,
                amount_cents: Some(amount_for_provider),
                currency: currency_for_provider.to_owned(),
                creator_account_id: stripe_account_id.to_owned(),
                platform_fee_cents: Some(fees.platform_fee_cents),
                success_url: format!("{base_url}/donations/{donation_id}/success"),
                cancel_url: format!("{base_url}/donations/{donation_id}/cancel"),
                metadata,
                price_id: None,
            },
        )
        .await
        .map_err(|e| match provider {
            "lightning" => MMError::Lightning(e.to_string()),
            _ => MMError::Stripe(e.to_string()),
        })?;

    // Insert donation row in PG (status: pending).
    let donation = Donation {
        id: donation_id,
        stream_id: req.stream_id,
        donor_user_id,
        recipient_user_id: stream.host_user_id,
        amount_cents: req.amount_cents,
        currency: "usd".to_owned(),
        message,
        tier: tier_info.name.to_owned(),
        pin_duration_secs: tier_info.pin_duration_secs as i32,
        stripe_session_id: Some(checkout_resp.session_id.clone()),
        stripe_payment_intent_id: None,
        status: DonationStatus::Pending.as_str().to_owned(),
        idempotency_key,
        created_at: chrono::Utc::now(),
    };
    db.create_donation(&donation).await?;

    state.metrics.donations_total.inc();

    // Only auto-complete in debug builds with mock provider.
    // In release builds, MockProvider donations stay "pending" -- operator must
    // manually approve via admin panel or use real Stripe webhooks.
    if cfg!(debug_assertions)
        && let Some(p) = registry.get("stripe")
        && p.is_mock()
    {
        tracing::info!(donation_id = %donation_id, "MockProvider: auto-completing donation (debug build only)");
        db.update_donation_status(
            donation.stripe_session_id.as_deref().unwrap_or_default(),
            mm_db::models::DonationStatus::Succeeded,
            Some("mock_pi_auto"),
        )
        .await
        .ok(); // Best-effort, don't fail the response
        state
            .metrics
            .donations_amount_cents_total
            .inc_by(req.amount_cents as u64);
    }

    // Lightning responses surface the BOLT11 + payment hash so the client can
    // render a QR and poll status. Stripe responses omit `invoice` entirely
    // (serde `skip_serializing_if`) — backward-compatible.
    let invoice = if provider == "lightning" {
        Some(LightningInvoice {
            bolt11: checkout_resp.checkout_url.clone(),
            payment_hash: checkout_resp.session_id.clone(),
            qr_data_url: None, // PR B (qrcode crate) lands separately.
        })
    } else {
        None
    };

    Ok(Json(CreateDonationResponse {
        donation_id,
        checkout_url: checkout_resp.checkout_url,
        invoice,
        tier: tier_info.name.to_owned(),
        pin_duration_secs: tier_info.pin_duration_secs,
    }))
}

// ---------------------------------------------------------------------------
// GET /streams/{id}/donations
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DonationFeedQuery {
    pub limit: Option<i64>,
    pub after: Option<String>, // ISO 8601 timestamp
}

#[derive(Debug, Serialize)]
pub struct DonationFeedResponse {
    pub donations: Vec<DonationDetailResponse>,
}

#[derive(Debug, Serialize)]
pub struct DonationDetailResponse {
    pub id: Uuid,
    pub stream_id: String,
    pub donor_display_name: String,
    pub amount_cents: i64,
    pub currency: String,
    pub message: Option<String>,
    pub tier: String,
    pub pin_duration_secs: i32,
    pub color: String,
    pub status: String,
    pub created_at: String,
}

/// Return recent successful donations for a stream (overlay feed).
pub async fn get_donation_feed(
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
    Query(query): Query<DonationFeedQuery>,
) -> Result<Json<DonationFeedResponse>, ApiError> {
    require_donations(&state)?;
    let db = db(&state);

    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let after = query
        .after
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let donations = db.get_donation_feed(&stream_id, limit, after).await?;

    let details: Vec<DonationDetailResponse> = donations
        .into_iter()
        .map(|d| {
            let tier_info = tier_for_amount(d.amount_cents);
            DonationDetailResponse {
                id: d.id,
                stream_id: d.stream_id,
                donor_display_name: d.donor_user_id, // Use user_id as display name for now
                amount_cents: d.amount_cents,
                currency: d.currency,
                message: d.message,
                tier: d.tier,
                pin_duration_secs: d.pin_duration_secs,
                color: tier_info.color.to_string(),
                status: d.status,
                created_at: d.created_at.to_rfc3339(),
            }
        })
        .collect();

    Ok(Json(DonationFeedResponse { donations: details }))
}

// ---------------------------------------------------------------------------
// POST /webhooks/stripe
// ---------------------------------------------------------------------------

/// Stripe webhook handler. Verifies signature, deduplicates, routes events.
///
/// This endpoint is UNAUTHENTICATED (no JWT) -- authentication is via Stripe
/// signature verification.
pub async fn stripe_webhook(
    State(state): State<SharedState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::http::StatusCode, ApiError> {
    require_monetization(&state)?;

    // Runtime guard: even if startup validation passed, verify the webhook
    // signing secret is still present and meets minimum length for HMAC security.
    let secret = &state.config.monetization.webhook_signing_secret;
    if secret.len() < 32 {
        tracing::error!("Webhook secret too short or empty -- rejecting all webhooks");
        return Err(
            MMError::api(ErrorCode::WebhookInvalid, "Webhook processing unavailable").into(),
        );
    }

    let db = db(&state);

    // Extract Stripe-Signature header.
    let sig = headers
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            MMError::api(ErrorCode::WebhookInvalid, "Missing Stripe-Signature header")
        })?;

    // Verify signature and parse event.
    let payload_str = std::str::from_utf8(&body)
        .map_err(|e| MMError::api(ErrorCode::WebhookInvalid, format!("Invalid UTF-8: {e}")))?;

    let event = stripe::Webhook::construct_event(
        payload_str,
        sig,
        &state.config.monetization.webhook_signing_secret,
    )
    .map_err(|e| {
        MMError::api(
            ErrorCode::WebhookInvalid,
            format!("Signature verification failed: {e}"),
        )
    })?;

    // Dedup: check if we already processed this event.
    let event_id = event.id.as_str();
    let event_type_str = event.type_.to_string();
    let is_new = db.record_webhook_event(event_id, &event_type_str).await?;
    if !is_new {
        tracing::debug!(event_id = %event_id, "Duplicate webhook event, ignoring");
        return Ok(axum::http::StatusCode::OK);
    }

    state.metrics.stripe_webhook_received_total.inc();

    // Route by event type.
    let result = match event.type_ {
        stripe::EventType::CheckoutSessionCompleted => {
            handle_checkout_completed(&state, db, &event).await
        }
        stripe::EventType::AccountUpdated => handle_account_updated(&state, db, &event).await,
        stripe::EventType::CustomerSubscriptionDeleted => {
            handle_subscription_deleted(&state, &event).await
        }
        other => {
            tracing::info!(event_type = %other, "Unhandled Stripe event type");
            Ok(())
        }
    };

    if let Err(e) = result {
        state.metrics.stripe_webhook_failed_total.inc();
        tracing::error!(event_id = %event_id, error = %e, "Webhook processing failed");
        // Still return 200 so Stripe doesn't retry.
        // We log the error and increment the failure counter.
    }

    Ok(axum::http::StatusCode::OK)
}

/// Handle checkout.session.completed: update donation OR subscription status
/// to active/succeeded depending on the session mode.
async fn handle_checkout_completed(
    state: &SharedState,
    db: &dyn Database,
    event: &stripe::Event,
) -> Result<(), MMError> {
    let session = match &event.data.object {
        stripe::EventObject::CheckoutSession(s) => s,
        _ => {
            return Err(MMError::Internal(
                "Expected CheckoutSession in checkout.session.completed".to_string(),
            ));
        }
    };

    let session_id = session.id.as_str();

    // Subscription mode: activate the pending subscription row. mm-core
    // inserts subscriptions with status='incomplete' and
    // `stripe_subscription_id=session_id`; the webhook flips that to 'active'
    // and, if the session carries a subscription id, swaps the column to the
    // real sub_* identifier.
    if matches!(session.mode, stripe::CheckoutSessionMode::Subscription) {
        return handle_subscription_checkout_completed(state, session, session_id).await;
    }

    // Extract payment_intent ID if available.
    let payment_intent_id: Option<String> = session
        .payment_intent
        .as_ref()
        .map(|pi| pi.id().as_str().to_owned());

    // Update donation status to succeeded.
    let donation = db
        .update_donation_status(
            session_id,
            DonationStatus::Succeeded,
            payment_intent_id.as_deref(),
        )
        .await?;

    if let Some(donation) = donation {
        state
            .metrics
            .donations_amount_cents_total
            .inc_by(donation.amount_cents as u64);

        // Emit Matrix donation event.
        // Look up the stream to find the room.
        if let Ok(Some(stream)) = state
            .db
            .get_stream(&StreamId(donation.stream_id.clone()))
            .await
        {
            // Look up room to get matrix_room_id.
            if let Ok(Some(room)) = state.db.get_room(stream.room_id).await {
                let tier_info = tier_for_amount(donation.amount_cents);
                let content = mm_matrix::events::DonationEventContent {
                    donation_id: donation.id.to_string(),
                    stream_id: donation.stream_id.clone(),
                    donor_display_name: donation.donor_user_id.clone(),
                    amount_cents: donation.amount_cents,
                    currency: donation.currency.clone(),
                    message: donation.message.clone(),
                    tier: donation.tier.clone(),
                    pin_duration_secs: tier_info.pin_duration_secs,
                    color: tier_info.color.to_string(),
                    version: 1,
                };

                match mm_matrix::events::emit_donation_event(
                    &state.hs_client,
                    &room.matrix_room_id,
                    &content,
                )
                .await
                {
                    Ok((donation_eid, notice_eid)) => {
                        tracing::info!(
                            donation_id = %donation.id,
                            donation_event_id = %donation_eid,
                            notice_event_id = %notice_eid,
                            "Donation event emitted to Matrix"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            donation_id = %donation.id,
                            error = %e,
                            "Failed to emit donation event to Matrix (non-fatal)"
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

/// Handle checkout.session.completed when the session is in subscription mode.
///
/// The subscription row was inserted at POST /subscriptions with
/// `status='incomplete'` and `stripe_subscription_id=<checkout session id>`.
/// Once the fake (or real) Stripe confirms the session, flip status to
/// 'active', extend the period end by 30 days, populate the real
/// `sub_*` id if the session includes one, and invalidate the entitlement
/// cache so the next gate check sees the fresh state.
async fn handle_subscription_checkout_completed(
    state: &SharedState,
    session: &stripe::CheckoutSession,
    session_id: &str,
) -> Result<(), MMError> {
    let pool = match pg_pool(state) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    // Try to pull a real subscription id out of the session if the payload
    // included one (fakestripe + real Stripe both do this).
    let real_sub_id: Option<String> = session
        .subscription
        .as_ref()
        .map(|s| s.id().as_str().to_owned());

    let new_sub_id = real_sub_id.clone().unwrap_or_else(|| session_id.to_owned());
    let period_end = chrono::Utc::now() + chrono::Duration::days(30);

    let result = sqlx::query_as::<_, SubscriptionActivationRow>(
        "UPDATE mm_subscriptions
         SET status = 'active',
             stripe_subscription_id = $1,
             current_period_end = $2,
             updated_at = now()
         WHERE stripe_subscription_id = $3 AND status = 'incomplete'
         RETURNING id, subscriber_user_id, creator_user_id",
    )
    .bind(&new_sub_id)
    .bind(period_end)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    if let Some(row) = result {
        tracing::info!(
            subscription_id = %row.id,
            subscriber = %row.subscriber_user_id,
            creator = %row.creator_user_id,
            stripe_sub_id = %new_sub_id,
            "Subscription activated via checkout.session.completed"
        );
        state.metrics.subscriptions_active.inc();

        // Invalidate entitlement cache so the fresh subscription is visible
        // immediately on the next check.
        if let Ok(ent_svc) = entitlement_service(state) {
            ent_svc
                .invalidate(&row.subscriber_user_id, &row.creator_user_id)
                .await;
        }
    } else {
        tracing::info!(
            session_id,
            "No incomplete subscription found for session; nothing to activate"
        );
    }

    Ok(())
}

/// Handle customer.subscription.deleted: mark the DB row cancelled.
async fn handle_subscription_deleted(
    state: &SharedState,
    event: &stripe::Event,
) -> Result<(), MMError> {
    let sub = match &event.data.object {
        stripe::EventObject::Subscription(s) => s,
        _ => {
            return Err(MMError::Internal(
                "Expected Subscription in customer.subscription.deleted".to_string(),
            ));
        }
    };

    let stripe_sub_id = sub.id.as_str();
    let pool = match pg_pool(state) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let row = sqlx::query_as::<_, SubscriptionActivationRow>(
        "UPDATE mm_subscriptions
         SET status = 'cancelled',
             cancelled_at = now(),
             updated_at = now()
         WHERE stripe_subscription_id = $1 AND status != 'cancelled'
         RETURNING id, subscriber_user_id, creator_user_id",
    )
    .bind(stripe_sub_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    if let Some(row) = row {
        tracing::info!(
            subscription_id = %row.id,
            stripe_sub_id,
            "Subscription cancelled via customer.subscription.deleted"
        );
        if let Ok(ent_svc) = entitlement_service(state) {
            ent_svc
                .invalidate(&row.subscriber_user_id, &row.creator_user_id)
                .await;
        }
    }

    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct SubscriptionActivationRow {
    id: Uuid,
    subscriber_user_id: String,
    creator_user_id: String,
}

/// Handle account.updated: update creator onboarding status.
async fn handle_account_updated(
    state: &SharedState,
    db: &dyn Database,
    event: &stripe::Event,
) -> Result<(), MMError> {
    let account = match &event.data.object {
        stripe::EventObject::Account(a) => a,
        _ => {
            return Err(MMError::Internal(
                "Expected Account in account.updated".to_string(),
            ));
        }
    };

    let account_id = account.id.as_str();
    let charges_enabled = account.charges_enabled.unwrap_or(false);

    if charges_enabled {
        db.set_creator_onboarding_complete(account_id, true).await?;
        state.metrics.creator_onboarding_total.inc();
        tracing::info!(
            stripe_account_id = %account_id,
            "Creator onboarding completed (charges_enabled)"
        );
    }

    Ok(())
}

// ===========================================================================
// Phase 7b: Subscription Endpoints
// ===========================================================================

// ---------------------------------------------------------------------------
// POST /creator/tiers -- Create a subscription tier
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateTierRequest {
    pub name: String,
    pub description: Option<String>,
    pub tier_level: i32,
    pub price_cents: i64,
    pub currency: Option<String>,
    pub perks: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct TierResponse {
    pub id: Uuid,
    /// `None` for platform-default tiers (available to all creators).
    pub creator_user_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub tier_level: i32,
    pub price_cents: i64,
    pub currency: String,
    pub stripe_price_id: Option<String>,
    pub perks: Vec<String>,
    pub active: bool,
    pub created_at: String,
}

/// Create a subscription tier for the authenticated creator.
///
/// Validates: creator onboarded, tier_level 1-5, price 99-4999, max 5 tiers.
/// Creates a Stripe Price via mm-payment and saves to DB.
pub async fn create_tier(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreateTierRequest>,
) -> Result<Json<TierResponse>, ApiError> {
    require_subscriptions(&state)?;
    let db = db(&state);
    let user_id = auth.user_id.0.as_str();

    // Verify creator is onboarded.
    let creator = db
        .get_creator_profile(user_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::CreatorNotOnboarded, "Creator profile not found"))?;

    if !creator.onboarding_complete {
        return Err(MMError::api(
            ErrorCode::CreatorNotOnboarded,
            "Complete Stripe onboarding first",
        )
        .into());
    }

    let _stripe_account_id = creator
        .stripe_account_id
        .as_deref()
        .ok_or_else(|| MMError::api(ErrorCode::CreatorNotOnboarded, "No Stripe account"))?;

    // Count existing active tiers for this creator.
    let existing_tiers = db.get_creator_tiers(user_id).await?;
    let existing_tier_count = existing_tiers.iter().filter(|t| t.is_active).count();

    // Validate tier parameters.
    subscriptions::validate_tier(req.tier_level, req.price_cents, existing_tier_count)
        .map_err(|msg| MMError::api(ErrorCode::InvalidAmount, msg))?;

    let perks = req.perks.unwrap_or_default();
    let perks_value = serde_json::to_value(&perks)
        .map_err(|e| MMError::Internal(format!("Failed to serialize perks: {e}")))?;

    // Insert tier via unified Database trait.
    let tier = state
        .db
        .create_tier(
            user_id,
            &req.name,
            req.price_cents,
            req.tier_level,
            req.description.as_deref(),
            Some(&perks_value),
            None,
        )
        .await?;

    // Try to create a real Stripe Price on the connected account.
    // Falls back to a synthetic `price_<tier_id>` if the Stripe client is
    // unavailable or the API call fails (e.g., fakestripe limitations).
    let stripe_price_id = if let Some(ref stripe_client) = state.stripe_client {
        let currency = req.currency.as_deref().unwrap_or("usd");
        let mut create_price = stripe::CreatePrice::new(stripe::Currency::USD);
        create_price.unit_amount = Some(req.price_cents);
        create_price.currency = currency.parse().unwrap_or(stripe::Currency::USD);
        create_price.recurring = Some(stripe::CreatePriceRecurring {
            interval: stripe::CreatePriceRecurringInterval::Month,
            ..Default::default()
        });
        create_price.product_data = Some(stripe::CreatePriceProductData {
            name: req.name.clone(),
            ..Default::default()
        });

        match stripe::Price::create(stripe_client, create_price).await {
            Ok(price) => {
                tracing::info!(
                    tier_id = %tier.id,
                    stripe_price_id = %price.id,
                    "Created real Stripe Price for tier"
                );
                price.id.to_string()
            }
            Err(e) => {
                tracing::warn!(
                    tier_id = %tier.id,
                    error = %e,
                    "Failed to create Stripe Price, using synthetic ID"
                );
                format!("price_{}", tier.id.simple())
            }
        }
    } else {
        // No Stripe client (mock provider or monetization via fakestripe only).
        format!("price_{}", tier.id.simple())
    };

    if let Ok(pool) = pg_pool(&state) {
        let _ = sqlx::query(
            "UPDATE mm_subscription_tiers SET stripe_price_id = $1 WHERE id = $2",
        )
        .bind(&stripe_price_id)
        .bind(tier.id)
        .execute(pool)
        .await;
    }

    let result_perks: Vec<String> =
        serde_json::from_value(tier.perks_json.clone()).unwrap_or_default();

    Ok(Json(TierResponse {
        id: tier.id,
        creator_user_id: tier.creator_user_id,
        name: tier.name,
        description: tier.description,
        tier_level: tier.tier_level,
        price_cents: tier.price_cents,
        currency: tier.currency,
        stripe_price_id: Some(stripe_price_id),
        perks: result_perks,
        active: tier.is_active,
        created_at: tier.created_at.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// PUT /creator/tiers/{tier_id} -- Update tier (name, description, perks only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdateTierRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub perks: Option<Vec<String>>,
}

/// Update a subscription tier's display fields (name, description, perks).
///
/// Price and tier_level are immutable after creation (Stripe constraint).
pub async fn update_tier(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(tier_id): Path<Uuid>,
    Json(req): Json<UpdateTierRequest>,
) -> Result<Json<TierResponse>, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    // Fetch existing tier and verify ownership.
    let tier = sqlx::query_as::<_, TierRow>(
        "SELECT id, creator_user_id, name, description, tier_level, price_cents, currency,
                stripe_price_id, perks_json, is_active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1",
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found"))?;

    if tier.creator_user_id.as_deref() != Some(user_id) {
        return Err(MMError::api(
            ErrorCode::Forbidden,
            "Not your tier (platform defaults are read-only; create your own to override)",
        )
        .into());
    }

    let new_name = req.name.as_deref().unwrap_or(&tier.name);
    let new_description = req.description.as_deref().or(tier.description.as_deref());
    let new_perks_json = if let Some(ref perks) = req.perks {
        serde_json::to_value(perks)
            .map_err(|e| MMError::Internal(format!("Failed to serialize perks: {e}")))?
    } else {
        tier.perks_json.clone()
    };

    sqlx::query(
        "UPDATE mm_subscription_tiers
         SET name = $1, description = $2, perks_json = $3, updated_at = now()
         WHERE id = $4",
    )
    .bind(new_name)
    .bind(new_description)
    .bind(&new_perks_json)
    .bind(tier_id)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let perks: Vec<String> = serde_json::from_value(new_perks_json).unwrap_or_default();

    Ok(Json(TierResponse {
        id: tier.id,
        creator_user_id: tier.creator_user_id,
        name: new_name.to_string(),
        description: new_description.map(|s| s.to_string()),
        tier_level: tier.tier_level,
        price_cents: tier.price_cents,
        currency: tier.currency,
        stripe_price_id: tier.stripe_price_id,
        perks,
        active: tier.is_active,
        created_at: tier.created_at.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// DELETE /creator/tiers/{tier_id} -- Deactivate tier
// ---------------------------------------------------------------------------

/// Deactivate a subscription tier (soft delete).
///
/// Existing subscribers remain active until their current period ends.
pub async fn delete_tier(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(tier_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    // Verify ownership.
    let tier = sqlx::query_as::<_, TierRow>(
        "SELECT id, creator_user_id, name, description, tier_level, price_cents, currency,
                stripe_price_id, perks_json, is_active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1",
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found"))?;

    if tier.creator_user_id.as_deref() != Some(user_id) {
        return Err(MMError::api(
            ErrorCode::Forbidden,
            "Not your tier (cannot delete platform defaults)",
        )
        .into());
    }

    sqlx::query(
        "UPDATE mm_subscription_tiers SET is_active = false, updated_at = now() WHERE id = $1",
    )
    .bind(tier_id)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /creators/{creator_id}/tiers -- List active tiers for a creator
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TierListResponse {
    pub tiers: Vec<TierResponse>,
}

/// List active subscription tiers for a creator (public endpoint).
pub async fn list_creator_tiers(
    State(state): State<SharedState>,
    Path(creator_id): Path<String>,
) -> Result<Json<TierListResponse>, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;

    // Show creator's own tiers first; for any tier_level the creator hasn't
    // overridden, fall back to the platform default (creator_user_id IS NULL).
    let rows = sqlx::query_as::<_, TierRow>(
        "WITH own AS (
             SELECT id, creator_user_id, name, description, tier_level, price_cents,
                    currency, stripe_price_id, perks_json, is_active, created_at
             FROM mm_subscription_tiers
             WHERE creator_user_id = $1 AND is_active = true
         )
         SELECT * FROM own
         UNION ALL
         SELECT id, creator_user_id, name, description, tier_level, price_cents,
                currency, stripe_price_id, perks_json, is_active, created_at
         FROM mm_subscription_tiers
         WHERE creator_user_id IS NULL AND is_active = true
           AND tier_level NOT IN (SELECT tier_level FROM own)
         ORDER BY tier_level ASC",
    )
    .bind(&creator_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let tiers = rows
        .into_iter()
        .map(|r| {
            let perks: Vec<String> =
                serde_json::from_value(r.perks_json.clone()).unwrap_or_default();
            TierResponse {
                id: r.id,
                creator_user_id: r.creator_user_id,
                name: r.name,
                description: r.description,
                tier_level: r.tier_level,
                price_cents: r.price_cents,
                currency: r.currency,
                stripe_price_id: r.stripe_price_id,
                perks,
                active: r.is_active,
                created_at: r.created_at.to_rfc3339(),
            }
        })
        .collect();

    Ok(Json(TierListResponse { tiers }))
}

// ---------------------------------------------------------------------------
// POST /subscriptions -- Subscribe to a tier
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionRequest {
    pub tier_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct CreateSubscriptionResponse {
    pub subscription_id: Uuid,
    pub checkout_url: String,
}

/// Subscribe to a creator's tier. Creates a Stripe Checkout session in
/// subscription mode and returns the checkout URL.
pub async fn create_subscription(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreateSubscriptionRequest>,
) -> Result<Json<CreateSubscriptionResponse>, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    // Fetch the tier.
    let tier = sqlx::query_as::<_, TierRow>(
        "SELECT id, creator_user_id, name, description, tier_level, price_cents, currency,
                stripe_price_id, perks_json, is_active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1 AND is_active = true",
    )
    .bind(req.tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found or inactive"))?;

    // Platform-default tiers must be adopted by a creator before they can be
    // subscribed to. Use POST /creator/tiers/adopt/{platform_tier_id}.
    let creator_user_id = tier.creator_user_id.as_deref().ok_or_else(|| {
        MMError::api(
            ErrorCode::NotFound,
            "Cannot subscribe to a platform-default tier directly; the creator must adopt it first",
        )
    })?;

    // Ensure the creator is onboarded.
    let db = db(&state);
    let creator = db
        .get_creator_profile(creator_user_id)
        .await?
        .ok_or_else(|| {
            MMError::api(
                ErrorCode::CreatorNotOnboarded,
                "Creator has not completed onboarding",
            )
        })?;

    if !creator.onboarding_complete {
        return Err(MMError::api(
            ErrorCode::CreatorNotOnboarded,
            "Creator has not completed Stripe onboarding",
        )
        .into());
    }

    let stripe_account_id = creator.stripe_account_id.as_deref().ok_or_else(|| {
        MMError::api(
            ErrorCode::CreatorNotOnboarded,
            "Creator has no Stripe account",
        )
    })?;

    // Generate subscription ID and build metadata.
    let subscription_id = Uuid::new_v4();
    let mut metadata = std::collections::HashMap::with_capacity(4);
    metadata.insert("subscription_id".to_owned(), subscription_id.to_string());
    metadata.insert("subscriber_user_id".to_owned(), user_id.to_owned());
    metadata.insert("creator_user_id".to_owned(), creator_user_id.to_owned());
    metadata.insert("tier_id".to_owned(), tier.id.to_string());

    // Calculate platform fee.
    let fees = calculate_fees(tier.price_cents, creator.platform_fee_pct);

    let base_url = state
        .config
        .server
        .public_url
        .as_deref()
        .unwrap_or("https://localhost:6167");
    let registry = payment_registry(&state)?;

    let checkout_resp = registry
        .create_checkout(
            "stripe",
            CheckoutRequest {
                mode: CheckoutMode::Subscription,
                amount_cents: Some(tier.price_cents),
                currency: tier.currency.clone(),
                creator_account_id: stripe_account_id.to_string(),
                platform_fee_cents: Some(fees.platform_fee_cents),
                success_url: format!("{base_url}/subscriptions/{subscription_id}/success"),
                cancel_url: format!("{base_url}/subscriptions/{subscription_id}/cancel"),
                metadata,
                price_id: tier.stripe_price_id.clone(),
            },
        )
        .await
        .map_err(|e| MMError::Stripe(e.to_string()))?;

    // Insert subscription row in PG (status: incomplete, awaiting checkout).
    // current_period_end is set to 1 month from now; it will be updated
    // by the webhook when Stripe confirms the subscription.
    let period_end = chrono::Utc::now() + chrono::Duration::days(30);
    sqlx::query(
        "INSERT INTO mm_subscriptions
            (id, subscriber_user_id, creator_user_id, tier_id, status,
             stripe_subscription_id, current_period_end, created_at)
         VALUES ($1, $2, $3, $4, 'incomplete', $5, $6, now())",
    )
    .bind(subscription_id)
    .bind(user_id)
    .bind(creator_user_id)
    .bind(tier.id)
    .bind(&checkout_resp.session_id)
    .bind(period_end)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(CreateSubscriptionResponse {
        subscription_id,
        checkout_url: checkout_resp.checkout_url,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /subscriptions/{id} -- Cancel subscription
// ---------------------------------------------------------------------------

/// Cancel an active subscription.
///
/// Updates DB status to 'cancelled'. The Stripe subscription will be cancelled
/// at period end so the user retains access until the current billing cycle.
pub async fn cancel_subscription(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(subscription_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    // Fetch and verify ownership (include stripe_subscription_id for API cancel).
    let sub = sqlx::query_as::<_, SubscriptionRow>(
        "SELECT subscriber_user_id, creator_user_id, status, stripe_subscription_id
         FROM mm_subscriptions
         WHERE id = $1",
    )
    .bind(subscription_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Subscription not found"))?;

    if sub.subscriber_user_id != user_id {
        return Err(MMError::api(ErrorCode::Forbidden, "Not your subscription").into());
    }

    if sub.status == "cancelled" {
        return Err(
            MMError::api(ErrorCode::InvalidAmount, "Subscription already cancelled").into(),
        );
    }

    // Cancel the subscription via Stripe API before updating the DB.
    // The stripe_subscription_id must start with "sub_" to be a real
    // Stripe subscription (vs. a checkout session ID or synthetic ID).
    if let Some(ref stripe_sub_id) = sub.stripe_subscription_id {
        if stripe_sub_id.starts_with("sub_") {
            if let Some(ref stripe_client) = state.stripe_client {
                let sub_id: stripe::SubscriptionId = stripe_sub_id
                    .parse()
                    .map_err(|_| MMError::Stripe("Invalid subscription ID format".to_string()))?;
                match stripe::Subscription::cancel(
                    stripe_client,
                    &sub_id,
                    stripe::CancelSubscription::default(),
                )
                .await
                {
                    Ok(_) => {
                        tracing::info!(
                            subscription_id = %subscription_id,
                            stripe_sub_id = %stripe_sub_id,
                            "Stripe subscription cancelled via API"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            subscription_id = %subscription_id,
                            stripe_sub_id = %stripe_sub_id,
                            error = %e,
                            "Failed to cancel Stripe subscription (proceeding with DB update)"
                        );
                    }
                }
            }
        }
    }

    sqlx::query(
        "UPDATE mm_subscriptions SET status = 'cancelled', cancelled_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(subscription_id)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    // Invalidate entitlement cache (H6: await synchronous L1 invalidation).
    if let Ok(ent_svc) = entitlement_service(&state) {
        ent_svc
            .invalidate(&sub.subscriber_user_id, &sub.creator_user_id)
            .await;
    }

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /subscriptions -- List own subscriptions
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SubscriptionResponse {
    pub id: Uuid,
    pub creator_user_id: String,
    pub tier_id: Uuid,
    pub tier_name: Option<String>,
    pub tier_level: Option<i32>,
    pub status: String,
    pub current_period_end: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct SubscriptionListResponse {
    pub subscriptions: Vec<SubscriptionResponse>,
}

/// List the authenticated user's subscriptions.
pub async fn list_subscriptions(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<SubscriptionListResponse>, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    let rows = sqlx::query_as::<_, SubscriptionWithTierRow>(
        "SELECT s.id, s.creator_user_id, s.tier_id, s.status,
                s.current_period_end, s.created_at,
                t.name AS tier_name, t.tier_level
         FROM mm_subscriptions s
         LEFT JOIN mm_subscription_tiers t ON t.id = s.tier_id
         WHERE s.subscriber_user_id = $1
         ORDER BY s.created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let subs = rows
        .into_iter()
        .map(|r| SubscriptionResponse {
            id: r.id,
            creator_user_id: r.creator_user_id,
            tier_id: r.tier_id,
            tier_name: r.tier_name,
            tier_level: r.tier_level,
            status: r.status,
            current_period_end: r.current_period_end.map(|dt| dt.to_rfc3339()),
            created_at: r.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(SubscriptionListResponse {
        subscriptions: subs,
    }))
}

// ---------------------------------------------------------------------------
// GET /subscriptions/check -- Check entitlement
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CheckEntitlementQuery {
    pub creator_user_id: String,
}

#[derive(Debug, Serialize)]
pub struct CheckEntitlementResponse {
    pub entitled: bool,
    pub tier_level: Option<i32>,
    pub tier_name: Option<String>,
    pub expires_at: Option<String>,
}

/// Check whether the authenticated user has an active subscription to a creator.
///
/// Uses the cached `EntitlementService` for fast lookups.
pub async fn check_entitlement(
    auth: AuthUser,
    State(state): State<SharedState>,
    Query(query): Query<CheckEntitlementQuery>,
) -> Result<Json<CheckEntitlementResponse>, ApiError> {
    require_subscriptions(&state)?;
    let ent_svc = entitlement_service(&state)?;
    let user_id = auth.user_id.0.as_str();

    let entitlement = ent_svc.check(user_id, &query.creator_user_id).await;

    match entitlement {
        Some(ent) => Ok(Json(CheckEntitlementResponse {
            entitled: true,
            tier_level: Some(ent.tier_level),
            tier_name: Some(ent.tier_name),
            expires_at: Some(ent.expires_at.to_rfc3339()),
        })),
        None => Ok(Json(CheckEntitlementResponse {
            entitled: false,
            tier_level: None,
            tier_name: None,
            expires_at: None,
        })),
    }
}

// ---------------------------------------------------------------------------
// Internal row types for subscription queries
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct TierRow {
    id: Uuid,
    creator_user_id: Option<String>,
    name: String,
    description: Option<String>,
    tier_level: i32,
    price_cents: i64,
    currency: String,
    stripe_price_id: Option<String>,
    perks_json: serde_json::Value,
    is_active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct SubscriptionRow {
    subscriber_user_id: String,
    creator_user_id: String,
    status: String,
    stripe_subscription_id: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct SubscriptionWithTierRow {
    id: Uuid,
    creator_user_id: String,
    tier_id: Uuid,
    status: String,
    current_period_end: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
    tier_name: Option<String>,
    tier_level: Option<i32>,
}

// ---------------------------------------------------------------------------
// Content Gates
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateGateRequest {
    pub content_type: String,
    pub content_id: String,
    pub min_tier_level: i32,
    pub preview_seconds: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct GateResponse {
    pub id: uuid::Uuid,
    pub content_type: String,
    pub content_id: String,
    pub creator_user_id: String,
    pub min_tier_level: i32,
    pub preview_seconds: i32,
}

pub async fn create_gate(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreateGateRequest>,
) -> Result<Json<GateResponse>, ApiError> {
    require_subscriptions(&state)?;
    let db = db(&state);

    // M5: Validate gate parameters
    mm_core::validation::validate_tier_level(req.min_tier_level)?;
    mm_core::validation::validate_preview_seconds(req.preview_seconds.unwrap_or(120))?;

    let gate = db
        .create_content_gate(
            &req.content_type,
            &req.content_id,
            &auth.user_id.0,
            req.min_tier_level,
            req.preview_seconds.unwrap_or(120),
        )
        .await?;

    Ok(Json(GateResponse {
        id: gate.id,
        content_type: gate.content_type,
        content_id: gate.content_id,
        creator_user_id: gate.creator_user_id,
        min_tier_level: gate.min_tier_level,
        preview_seconds: gate.preview_seconds,
    }))
}

pub async fn get_gate(
    State(state): State<SharedState>,
    Path((content_type, content_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_subscriptions(&state)?;
    let db = db(&state);

    match db.get_content_gate(&content_type, &content_id).await? {
        Some(gate) => Ok(Json(serde_json::json!({
            "id": gate.id,
            "content_type": gate.content_type,
            "content_id": gate.content_id,
            "creator_user_id": gate.creator_user_id,
            "min_tier_level": gate.min_tier_level,
            "preview_seconds": gate.preview_seconds,
        }))),
        None => Ok(Json(serde_json::json!({"gate": null}))),
    }
}

pub async fn delete_gate(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path((content_type, content_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_subscriptions(&state)?;
    let db = db(&state);
    db.delete_content_gate(&content_type, &content_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// Route builders
// ---------------------------------------------------------------------------

/// Authenticated monetization routes (nested under `/_mm/client/v1/`).
pub fn routes(state: SharedState) -> axum::Router {
    use axum::routing::{delete, get, post, put};

    axum::Router::new()
        // Phase 7a: Donations
        .route("/creator/onboard", post(creator_onboard))
        .route("/creator/profile", get(get_creator_profile))
        .route("/donations", post(create_donation))
        .route("/streams/{stream_id}/donations", get(get_donation_feed))
        // Phase 7b: Subscriptions
        .route("/creator/tiers", post(create_tier))
        .route("/creator/tiers/{tier_id}", put(update_tier))
        .route("/creator/tiers/{tier_id}", delete(delete_tier))
        .route("/creators/{creator_id}/tiers", get(list_creator_tiers))
        .route("/subscriptions", post(create_subscription))
        .route("/subscriptions", get(list_subscriptions))
        .route("/subscriptions/check", get(check_entitlement))
        .route("/subscriptions/{id}", delete(cancel_subscription))
        // Lightning payment status
        .route("/payments/lightning/{hash}", get(check_lightning_payment))
        // Content gates
        .route("/gates", post(create_gate))
        .route("/gates/{content_type}/{content_id}", get(get_gate))
        .route("/gates/{content_type}/{content_id}", delete(delete_gate))
        .with_state(state)
}

/// Lightning payment status response — what the web client polls during the
/// invoice-display window. Per `m1-pilot-kickoff.md` BACKEND-04.
///
/// `status` is one of `pending` | `succeeded` | `failed` | `unknown`.
/// `paid_at` + `preimage` populated only on `succeeded`.
#[derive(Debug, Serialize)]
pub struct LightningPaymentStatusResponse {
    pub payment_hash: String,
    pub paid: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preimage: Option<String>,
}

/// GET `/payments/lightning/:hash` — Check Lightning payment status.
///
/// Web client polls this every ~2s during the invoice-display window.
/// Source-of-truth is the `mm_donations` table — the LNBits webhook
/// updates the row to `succeeded` when the invoice is paid (see
/// `lnbits_webhook` handler). If the row is missing entirely we report
/// `unknown` so the client can decide to retry or surface "expired".
async fn check_lightning_payment(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(hash): Path<String>,
) -> Result<Json<LightningPaymentStatusResponse>, ApiError> {
    require_monetization(&state)?;

    // Confirm Lightning is at least configured for this operator. Returning
    // FeatureDisabled before doing the DB lookup avoids leaking row existence.
    let registry = payment_registry(&state)?;
    if registry.get("lightning").is_none() {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "Lightning payments not enabled",
        )
        .into());
    }

    let Some(pool) = state.pg_pool.as_ref() else {
        return Ok(Json(LightningPaymentStatusResponse {
            payment_hash: hash,
            paid: false,
            status: "unknown".to_owned(),
            paid_at: None,
            preimage: None,
        }));
    };

    // We only have status + updated_at on the existing schema — preimage
    // tracking is a future schema bump (M3). For pilot, paid_at falls back
    // to created_at so the client at least gets a timestamp.
    let row: Option<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT status, created_at FROM mm_donations \
         WHERE provider_payment_id = $1 LIMIT 1",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let (status, paid_at) = match row {
        Some((s, ts)) => {
            let paid = s == "succeeded";
            (s, paid.then(|| ts.to_rfc3339()))
        }
        None => ("unknown".to_owned(), None),
    };

    Ok(Json(LightningPaymentStatusResponse {
        payment_hash: hash,
        paid: status == "succeeded",
        status,
        paid_at,
        preimage: None, // Schema bump in M3 will surface preimage from LNBits webhook.
    }))
}

/// Webhook routes (unauthenticated, nested under `/_mm/webhooks/`).
pub fn webhook_routes(state: SharedState) -> axum::Router {
    use axum::routing::post;

    axum::Router::new()
        .route("/stripe", post(stripe_webhook))
        .route("/lnbits", post(lnbits_webhook))
        .with_state(state)
}

/// LNBits webhook handler. Called when a Lightning payment is received.
async fn lnbits_webhook(
    State(state): State<SharedState>,
    body: axum::body::Bytes,
) -> Result<axum::Json<serde_json::Value>, crate::error::ApiError> {
    let registry = state.payment_registry.as_ref()
        .ok_or_else(|| mm_core::error::MMError::api(mm_core::error::ErrorCode::MonetizationDisabled, "monetization disabled"))?;

    let event = registry.verify_webhook("lightning", &body, "").await
        .map_err(|e| mm_core::error::MMError::Internal(format!("LNBits webhook error: {e}")))?;

    // Process the webhook event (same as Stripe flow)
    match event {
        mm_payment::WebhookEvent::CheckoutCompleted { session_id, metadata, .. } => {
            let donation_id = metadata.get("donation_id").cloned().unwrap_or_default();
            let stream_id = metadata.get("stream_id").cloned().unwrap_or_default();
            let amount_sats: i64 = metadata.get("amount_sats")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            tracing::info!(
                payment_hash = %session_id,
                donation_id = %donation_id,
                stream_id = %stream_id,
                amount_sats = amount_sats,
                "Lightning payment received"
            );

            // Update donation status in DB.
            // Use 'succeeded' to match the CHECK constraint on mm_donations.status.
            // Parse donation_id as UUID since mm_donations.id is UUID.
            if let Some(pool) = state.pg_pool.as_ref() {
                if let Ok(parsed_id) = donation_id.parse::<Uuid>() {
                    let _ = sqlx::query(
                        "UPDATE mm_donations SET status = 'succeeded', provider_payment_id = $1 WHERE id = $2"
                    )
                    .bind(&session_id)
                    .bind(parsed_id)
                    .execute(pool)
                    .await;
                } else {
                    tracing::warn!(donation_id = %donation_id, "LNBits webhook: invalid donation_id UUID");
                }
            }

            // Emit Matrix donation event (same as Stripe flow)
            if !stream_id.is_empty() && !donation_id.is_empty() {
                if let Some(pool) = state.pg_pool.as_ref() {
                    let donor = metadata.get("donor_user_id").cloned().unwrap_or_default();
                    let message = metadata.get("message").cloned();
                    let amount_cents = mm_payment::lnbits::types::sats_to_usd_cents(amount_sats);

                    // Emit donation event to Matrix room
                    if let Ok(Some(stream)) = state.db.get_stream(&mm_core::types::StreamId(stream_id.clone())).await {
                        if let Ok(Some(room)) = state.db.get_room(stream.room_id).await {
                            let tier = mm_payment::tier_for_amount(amount_cents);
                            let content = mm_matrix::events::DonationEventContent {
                                donation_id: donation_id.clone(),
                                stream_id: stream_id.clone(),
                                donor_display_name: donor.clone(),
                                amount_cents,
                                currency: "sats".to_string(),
                                message,
                                tier: tier.name.to_string(),
                                pin_duration_secs: tier.pin_duration_secs,
                                color: tier.color.to_string(),
                                version: 1,
                            };
                            let _ = mm_matrix::events::emit_donation_event(
                                &state.hs_client,
                                &room.matrix_room_id,
                                &content,
                            ).await;
                        }
                    }
                }
            }
        }
        _ => {
            tracing::debug!("LNBits webhook: unhandled event type");
        }
    }

    Ok(axum::Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn donation_request_defaults_to_stripe() {
        let json = r#"{"stream_id": "s1", "amount_cents": 500}"#;
        let req: CreateDonationRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.payment_provider, "stripe");
    }

    #[test]
    fn donation_request_accepts_lightning() {
        let json = r#"{"stream_id": "s1", "amount_cents": 500, "payment_provider": "lightning"}"#;
        let req: CreateDonationRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.payment_provider, "lightning");
    }

    #[test]
    fn donation_response_stripe_omits_invoice_field() {
        let resp = CreateDonationResponse {
            donation_id: Uuid::new_v4(),
            checkout_url: "https://checkout.stripe.com/pay/cs_test_abc".to_owned(),
            invoice: None,
            tier: "bronze".to_owned(),
            pin_duration_secs: 60,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("invoice"),
            "Stripe response must NOT contain `invoice` field for backward compatibility"
        );
    }

    #[test]
    fn lightning_payment_status_pending_omits_optional_fields() {
        let resp = LightningPaymentStatusResponse {
            payment_hash: "abc123".to_owned(),
            paid: false,
            status: "pending".to_owned(),
            paid_at: None,
            preimage: None,
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["paid"], false);
        assert_eq!(value["status"], "pending");
        assert!(value.get("paid_at").is_none(), "paid_at must be omitted when None");
        assert!(value.get("preimage").is_none(), "preimage must be omitted when None");
    }

    #[test]
    fn lightning_payment_status_succeeded_includes_paid_at() {
        let resp = LightningPaymentStatusResponse {
            payment_hash: "abc123".to_owned(),
            paid: true,
            status: "succeeded".to_owned(),
            paid_at: Some("2026-04-26T10:00:00Z".to_owned()),
            preimage: None,
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["paid"], true);
        assert_eq!(value["paid_at"], "2026-04-26T10:00:00Z");
    }

    #[test]
    fn donation_response_lightning_includes_invoice() {
        let resp = CreateDonationResponse {
            donation_id: Uuid::new_v4(),
            checkout_url: "lnbc500m1pwabcdef...".to_owned(),
            invoice: Some(LightningInvoice {
                bolt11: "lnbc500m1pwabcdef...".to_owned(),
                payment_hash: "abc123def456".to_owned(),
                qr_data_url: None,
            }),
            tier: "gold".to_owned(),
            pin_duration_secs: 300,
        };
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            value["invoice"]["bolt11"].as_str().unwrap(),
            "lnbc500m1pwabcdef..."
        );
        assert_eq!(
            value["invoice"]["payment_hash"].as_str().unwrap(),
            "abc123def456"
        );
        // qr_data_url omitted when None
        assert!(value["invoice"].get("qr_data_url").is_none());
    }
}
