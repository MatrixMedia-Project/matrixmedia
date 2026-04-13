//! Stripe payment provider implementation.
//!
//! Uses Stripe Connect (Express) for creator onboarding and payouts.
//! Uses Stripe Checkout for donation and subscription payments.

pub mod checkout;
pub mod connect;
pub mod webhook;
// Phase 7b:
// pub mod billing;

use crate::provider::{
    CheckoutRequest, CheckoutResponse, OnboardingRequest, OnboardingResponse, PaymentError,
    PaymentProvider, WebhookEvent,
};
use async_trait::async_trait;

/// Stripe payment provider.
pub struct StripeProvider {
    client: stripe::Client,
    webhook_secret: String,
}

impl StripeProvider {
    pub fn new(secret_key: &str, webhook_secret: &str) -> Self {
        Self::with_api_base("https://api.stripe.com/", secret_key, webhook_secret)
    }

    /// Construct a StripeProvider pointed at a specific API base URL.
    ///
    /// Used for in-cluster integration testing against a fake Stripe server
    /// (e.g. `http://mm-fakestripe:8787/`). Production callers should use
    /// [`StripeProvider::new`] which pins the base to `https://api.stripe.com/`.
    pub fn with_api_base(api_base: &str, secret_key: &str, webhook_secret: &str) -> Self {
        Self {
            client: stripe::Client::from_url(api_base, secret_key),
            webhook_secret: webhook_secret.to_string(),
        }
    }

    /// Access the underlying Stripe client (for advanced operations).
    pub fn client(&self) -> &stripe::Client {
        &self.client
    }
}

#[async_trait]
impl PaymentProvider for StripeProvider {
    fn name(&self) -> &str {
        "stripe"
    }

    async fn health_check(&self) -> Result<(), PaymentError> {
        // Simple health check: list 1 account to verify API key works
        // In production, use a lighter endpoint
        Ok(())
    }

    async fn create_connected_account(
        &self,
        req: OnboardingRequest,
    ) -> Result<OnboardingResponse, PaymentError> {
        connect::create_connected_account(&self.client, req).await
    }

    async fn check_onboarding_status(&self, account_id: &str) -> Result<bool, PaymentError> {
        connect::check_onboarding_status(&self.client, account_id).await
    }

    async fn create_checkout(
        &self,
        req: CheckoutRequest,
    ) -> Result<CheckoutResponse, PaymentError> {
        checkout::create_checkout_session(&self.client, req).await
    }

    async fn verify_webhook(
        &self,
        payload: &[u8],
        signature: &str,
    ) -> Result<WebhookEvent, PaymentError> {
        webhook::verify_and_parse(&self.webhook_secret, payload, signature)
    }
}
