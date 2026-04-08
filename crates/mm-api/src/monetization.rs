//! Monetization API handlers (Phase 7a: Donations).
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
use mm_db::MonetizationDb;
use mm_db::models::{Donation, DonationStatus};
use mm_db::monetization_db::PgMonetizationDb;
use mm_payment::donations::{calculate_fees, tier_for_amount};
use mm_payment::provider::{CheckoutMode, CheckoutRequest, OnboardingRequest};

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

/// Build a PgMonetizationDb from the shared state's pool.
fn monetization_db(state: &SharedState) -> Result<PgMonetizationDb, MMError> {
    let pool = pg_pool(state)?;
    Ok(PgMonetizationDb::new(pool.clone()))
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
    let db = monetization_db(&state)?;

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
    let db = monetization_db(&state)?;

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
    let db = monetization_db(&state)?;

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
    let db = monetization_db(&state)?;

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
    let db = monetization_db(&state)?;

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
            handle_checkout_completed(&state, &db, &event).await
        }
        stripe::EventType::AccountUpdated => handle_account_updated(&state, &db, &event).await,
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
    db: &PgMonetizationDb,
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
    db: &PgMonetizationDb,
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

// ---------------------------------------------------------------------------
// Route builders
// ---------------------------------------------------------------------------

/// Authenticated monetization routes (nested under `/_mm/client/v1/`).
pub fn routes(state: SharedState) -> axum::Router {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/creator/onboard", post(creator_onboard))
        .route("/creator/profile", get(get_creator_profile))
        .route("/donations", post(create_donation))
        .route("/streams/{stream_id}/donations", get(get_donation_feed))
        .with_state(state)
}

/// Webhook routes (unauthenticated, nested under `/_mm/webhooks/`).
pub fn webhook_routes(state: SharedState) -> axum::Router {
    use axum::routing::post;

    axum::Router::new()
        .route("/stripe", post(stripe_webhook))
        .with_state(state)
}
