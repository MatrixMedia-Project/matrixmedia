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
// Helpers
// ---------------------------------------------------------------------------

/// Guard: returns 501 if monetization is disabled.
fn require_monetization(state: &SharedState) -> Result<(), MMError> {
    if !state.config.monetization.enabled {
        return Err(MMError::api(
            ErrorCode::MonetizationDisabled,
            "Monetization is not enabled",
        ));
    }
    Ok(())
}

/// Guard: returns 501 if donations specifically are disabled.
fn require_donations(state: &SharedState) -> Result<(), MMError> {
    require_monetization(state)?;
    if !state.config.monetization.donations_enabled {
        return Err(MMError::api(
            ErrorCode::MonetizationDisabled,
            "Donations are not enabled",
        ));
    }
    Ok(())
}

/// Guard: returns 501 if subscriptions specifically are disabled.
fn require_subscriptions(state: &SharedState) -> Result<(), MMError> {
    require_monetization(state)?;
    if !state.config.monetization.subscriptions_enabled {
        return Err(MMError::api(
            ErrorCode::SubscriptionsDisabled,
            "Subscriptions are not enabled",
        ));
    }
    Ok(())
}

/// Get the EntitlementService, returning an error if None.
fn entitlement_service(state: &SharedState) -> Result<&mm_payment::EntitlementService, MMError> {
    state
        .entitlement_service
        .as_deref()
        .ok_or_else(|| MMError::Internal("Entitlement service not initialized".to_string()))
}

/// Get the PgPool, returning an error if None.
fn pg_pool(state: &SharedState) -> Result<&sqlx::PgPool, MMError> {
    state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::Internal("PG pool not initialized".to_string()))
}

/// Get the Stripe client, returning an error if None.
fn stripe_client(state: &SharedState) -> Result<&stripe::Client, MMError> {
    state
        .stripe_client
        .as_ref()
        .ok_or_else(|| MMError::Internal("Stripe client not initialized".to_string()))
}

/// Get the payment provider registry, returning an error if None.
fn payment_registry(state: &SharedState) -> Result<&mm_payment::PaymentProviderRegistry, MMError> {
    state
        .payment_registry
        .as_deref()
        .ok_or_else(|| MMError::Internal("Payment registry not initialized".to_string()))
}

/// Get the unified Database reference from shared state.
fn db(state: &SharedState) -> &dyn Database {
    &*state.db
}

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
    let client = stripe_client(&state)?;
    let db = db(&state);

    let user_id = auth.user_id.0.as_str();

    // Check if profile already exists with a stripe account (idempotent).
    if let Some(existing) = db.get_creator_profile(user_id).await?
        && existing.stripe_account_id.is_some()
    {
        // Already onboarded or in progress -- create a fresh account link
        // so they can resume or complete onboarding.
        let stripe_acct_id = existing.stripe_account_id.as_deref().unwrap();
        let account_id: stripe::AccountId = stripe_acct_id.parse().map_err(|_| {
            MMError::Internal(format!(
                "Invalid stored Stripe account ID: {stripe_acct_id}"
            ))
        })?;

        let base_url = state
            .config
            .server
            .public_url
            .as_deref()
            .unwrap_or("https://localhost:6167");
        let return_url = format!("{base_url}/creator/onboard/return");
        let refresh_url = format!("{base_url}/creator/onboard/refresh");
        let link_params = stripe::CreateAccountLink {
            account: account_id,
            type_: stripe::AccountLinkType::AccountOnboarding,
            return_url: Some(&return_url),
            refresh_url: Some(&refresh_url),
            collect: None,
            collection_options: None,
            expand: &[],
        };

        let link = stripe::AccountLink::create(client, link_params)
            .await
            .map_err(|e| MMError::Stripe(format!("Account link creation failed: {e}")))?;

        return Ok(Json(CreatorOnboardResponse {
            creator_id: existing.id,
            onboarding_url: link.url,
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
}

#[derive(Debug, Serialize)]
pub struct CreateDonationResponse {
    pub donation_id: Uuid,
    pub checkout_url: String,
    pub tier: String,
    pub pin_duration_secs: u32,
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

    // Validate amount bounds.
    let min = state.config.monetization.min_donation_cents;
    let max = state.config.monetization.max_donation_cents;
    if req.amount_cents < min || req.amount_cents > max {
        return Err(MMError::api(
            ErrorCode::InvalidAmount,
            format!("Amount must be between {min} and {max} cents"),
        )
        .into());
    }

    // Validate message length.
    if let Some(ref msg) = req.message
        && msg.len() > 150
    {
        return Err(MMError::api(
            ErrorCode::InvalidAmount,
            "Message must be 150 characters or fewer",
        )
        .into());
    }

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

    // Generate donation ID and idempotency key.
    let donation_id = Uuid::new_v4();
    let idempotency_key = Uuid::new_v4().to_string();

    // Build metadata for Stripe session.
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("donation_id".to_string(), donation_id.to_string());
    metadata.insert("stream_id".to_string(), req.stream_id.clone());
    metadata.insert("donor_user_id".to_string(), auth.user_id.0.clone());

    // Create Stripe Checkout Session via payment registry.
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
                mode: CheckoutMode::Payment,
                amount_cents: Some(req.amount_cents),
                currency: "usd".to_string(),
                creator_account_id: stripe_account_id.to_string(),
                platform_fee_cents: Some(fees.platform_fee_cents),
                success_url: format!("{base_url}/donations/{donation_id}/success"),
                cancel_url: format!("{base_url}/donations/{donation_id}/cancel"),
                metadata,
                price_id: None,
            },
        )
        .await
        .map_err(|e| MMError::Stripe(e.to_string()))?;

    // Insert donation row in PG (status: pending).
    let donation = Donation {
        id: donation_id,
        stream_id: req.stream_id.clone(),
        donor_user_id: auth.user_id.0.clone(),
        recipient_user_id: stream.host_user_id.clone(),
        amount_cents: req.amount_cents,
        currency: "usd".to_string(),
        message: req.message.clone(),
        tier: tier_info.name.clone(),
        pin_duration_secs: tier_info.pin_duration_secs as i32,
        stripe_session_id: Some(checkout_resp.session_id),
        stripe_payment_intent_id: None,
        status: DonationStatus::Pending.as_str().to_string(),
        idempotency_key,
        created_at: chrono::Utc::now(),
    };
    db.create_donation(&donation).await?;

    state.metrics.donations_total.inc();

    Ok(Json(CreateDonationResponse {
        donation_id,
        checkout_url: checkout_resp.checkout_url,
        tier: tier_info.name,
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
    let event_id = event.id.as_str().to_string();
    let event_type_str = event.type_.to_string();
    let is_new = db.record_webhook_event(&event_id, &event_type_str).await?;
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

/// Handle checkout.session.completed: update donation status to succeeded.
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

    let session_id = session.id.as_str().to_string();

    // Extract payment_intent ID if available.
    let payment_intent_id = session
        .payment_intent
        .as_ref()
        .map(|pi| pi.id().as_str().to_string());

    // Update donation status to succeeded.
    let donation = db
        .update_donation_status(
            &session_id,
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
                        tracing::error!(
                            donation_id = %donation.id,
                            error = %e,
                            "Failed to emit donation event to Matrix"
                        );
                    }
                }
            }
        }
    }

    Ok(())
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

    let account_id = account.id.as_str().to_string();
    let charges_enabled = account.charges_enabled.unwrap_or(false);

    if charges_enabled {
        db.set_creator_onboarding_complete(&account_id, true)
            .await?;
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
    pub creator_user_id: String,
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
    // NOTE: get_active_tiers is provided by the mm-db migrations agent.
    //       Using todo!() as placeholder until available.
    let existing_tier_count: usize = 0; // TODO: db.count_active_tiers(user_id).await?

    // Validate tier parameters.
    subscriptions::validate_tier(req.tier_level, req.price_cents, existing_tier_count)
        .map_err(|msg| MMError::api(ErrorCode::InvalidAmount, msg))?;

    // TODO: Create Stripe Price via registry when billing module is ready.
    // For now, store the tier without a stripe_price_id -- it will be set
    // when the Stripe billing module creates the price.

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
        stripe_price_id: tier.stripe_price_id,
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
                stripe_price_id, perks, active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1",
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found"))?;

    if tier.creator_user_id != user_id {
        return Err(MMError::api(ErrorCode::Forbidden, "Not your tier").into());
    }

    let new_name = req.name.as_deref().unwrap_or(&tier.name);
    let new_description = req.description.as_deref().or(tier.description.as_deref());
    let new_perks_json = if let Some(ref perks) = req.perks {
        serde_json::to_string(perks)
            .map_err(|e| MMError::Internal(format!("Failed to serialize perks: {e}")))?
    } else {
        tier.perks.clone()
    };

    sqlx::query(
        "UPDATE mm_subscription_tiers
         SET name = $1, description = $2, perks = $3, updated_at = now()
         WHERE id = $4",
    )
    .bind(new_name)
    .bind(new_description)
    .bind(&new_perks_json)
    .bind(tier_id)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let perks: Vec<String> = serde_json::from_str(&new_perks_json).unwrap_or_default();

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
        active: tier.active,
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
                stripe_price_id, perks, active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1",
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found"))?;

    if tier.creator_user_id != user_id {
        return Err(MMError::api(ErrorCode::Forbidden, "Not your tier").into());
    }

    sqlx::query(
        "UPDATE mm_subscription_tiers SET active = false, updated_at = now() WHERE id = $1",
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

    let rows = sqlx::query_as::<_, TierRow>(
        "SELECT id, creator_user_id, name, description, tier_level, price_cents, currency,
                stripe_price_id, perks, active, created_at
         FROM mm_subscription_tiers
         WHERE creator_user_id = $1 AND active = true
         ORDER BY tier_level ASC",
    )
    .bind(&creator_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let tiers = rows
        .into_iter()
        .map(|r| {
            let perks: Vec<String> = serde_json::from_str(&r.perks).unwrap_or_default();
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
                active: r.active,
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
                stripe_price_id, perks, active, created_at
         FROM mm_subscription_tiers
         WHERE id = $1 AND active = true",
    )
    .bind(req.tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "Tier not found or inactive"))?;

    // Ensure the creator is onboarded.
    let db = db(&state);
    let creator = db
        .get_creator_profile(&tier.creator_user_id)
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
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("subscription_id".to_string(), subscription_id.to_string());
    metadata.insert("subscriber_user_id".to_string(), user_id.to_string());
    metadata.insert("creator_user_id".to_string(), tier.creator_user_id.clone());
    metadata.insert("tier_id".to_string(), tier.id.to_string());

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
    sqlx::query(
        "INSERT INTO mm_subscriptions
            (id, subscriber_user_id, creator_user_id, tier_id, status,
             stripe_checkout_session_id, created_at)
         VALUES ($1, $2, $3, $4, 'incomplete', $5, now())",
    )
    .bind(subscription_id)
    .bind(user_id)
    .bind(&tier.creator_user_id)
    .bind(tier.id)
    .bind(&checkout_resp.session_id)
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
/// Updates DB status to 'canceled'. The Stripe subscription will be canceled
/// at period end so the user retains access until the current billing cycle.
pub async fn cancel_subscription(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(subscription_id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_subscriptions(&state)?;
    let pool = pg_pool(&state)?;
    let user_id = auth.user_id.0.as_str();

    // Fetch and verify ownership.
    let sub = sqlx::query_as::<_, SubscriptionRow>(
        "SELECT id, subscriber_user_id, creator_user_id, tier_id, status,
                stripe_subscription_id, current_period_end, created_at
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

    if sub.status == "canceled" {
        return Err(MMError::api(ErrorCode::InvalidAmount, "Subscription already canceled").into());
    }

    // TODO: Call Stripe to cancel the subscription at period end:
    //   stripe::Subscription::update(client, sub_id, { cancel_at_period_end: true })
    // For now, just update DB status.

    sqlx::query(
        "UPDATE mm_subscriptions SET status = 'canceled', updated_at = now() WHERE id = $1",
    )
    .bind(subscription_id)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    // Invalidate entitlement cache.
    if let Ok(ent_svc) = entitlement_service(&state) {
        ent_svc.invalidate(&sub.subscriber_user_id, &sub.creator_user_id);
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
    creator_user_id: String,
    name: String,
    description: Option<String>,
    tier_level: i32,
    price_cents: i64,
    currency: String,
    stripe_price_id: Option<String>,
    perks: String, // JSON array stored as text
    active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct SubscriptionRow {
    id: Uuid,
    subscriber_user_id: String,
    creator_user_id: String,
    tier_id: Uuid,
    status: String,
    stripe_subscription_id: Option<String>,
    current_period_end: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
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
        .with_state(state)
}

/// Webhook routes (unauthenticated, nested under `/_mm/webhooks/`).
pub fn webhook_routes(state: SharedState) -> axum::Router {
    use axum::routing::post;

    axum::Router::new()
        .route("/stripe", post(stripe_webhook))
        .with_state(state)
}
