//! Payment provider abstraction trait.
//!
//! Phase 7a: Only `StripeProvider` is implemented.
//! Phase 8: Add `PayPalProvider`, `CryptoProvider`, etc.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Errors from payment providers.
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("Provider unavailable: {0}")]
    Unavailable(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Payment failed: {0}")]
    PaymentFailed(String),

    #[error("Webhook verification failed: {0}")]
    WebhookInvalid(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Provider error: {0}")]
    Internal(String),
}

/// Request to create a checkout session (donation or subscription).
#[derive(Debug, Clone)]
pub struct CheckoutRequest {
    pub mode: CheckoutMode,
    pub amount_cents: Option<i64>,
    pub currency: String,
    pub creator_account_id: String,
    pub platform_fee_cents: Option<i64>,
    pub success_url: String,
    pub cancel_url: String,
    pub metadata: HashMap<String, String>,
    /// For subscription mode: the provider-specific price ID.
    pub price_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutMode {
    Payment,
    Subscription,
}

/// Response from creating a checkout session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutResponse {
    pub session_id: String,
    pub checkout_url: String,
}

/// Request to onboard a creator (connect their payment account).
#[derive(Debug, Clone)]
pub struct OnboardingRequest {
    pub user_id: String,
    pub return_url: String,
    pub refresh_url: String,
}

/// Response from onboarding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingResponse {
    pub account_id: String,
    pub onboarding_url: String,
}

/// Normalized webhook event from any provider.
#[derive(Debug, Clone)]
pub enum WebhookEvent {
    CheckoutCompleted {
        session_id: String,
        mode: CheckoutMode,
        metadata: HashMap<String, String>,
    },
    AccountOnboarded {
        account_id: String,
        charges_enabled: bool,
    },
    SubscriptionCreated {
        subscription_id: String,
        customer_id: String,
        metadata: HashMap<String, String>,
    },
    SubscriptionUpdated {
        subscription_id: String,
        status: String,
    },
    SubscriptionCancelled {
        subscription_id: String,
    },
    PaymentFailed {
        subscription_id: Option<String>,
        reason: String,
    },
    Unknown {
        event_type: String,
    },
}

/// Payment provider trait -- implement for each payment system.
///
/// Same pattern as `SfuAdapter` in mm-sfu.
#[async_trait]
pub trait PaymentProvider: Send + Sync + 'static {
    /// Provider name (e.g., "stripe", "paypal", "btcpay").
    fn name(&self) -> &str;

    /// Health check -- can we reach the provider?
    async fn health_check(&self) -> Result<(), PaymentError>;

    /// Create a connected account for a creator.
    async fn create_connected_account(
        &self,
        req: OnboardingRequest,
    ) -> Result<OnboardingResponse, PaymentError>;

    /// Check if a connected account has completed onboarding.
    async fn check_onboarding_status(&self, account_id: &str) -> Result<bool, PaymentError>;

    /// Create a checkout session (payment or subscription).
    async fn create_checkout(&self, req: CheckoutRequest)
    -> Result<CheckoutResponse, PaymentError>;

    /// Verify and parse a webhook payload.
    async fn verify_webhook(
        &self,
        payload: &[u8],
        signature: &str,
    ) -> Result<WebhookEvent, PaymentError>;
}
