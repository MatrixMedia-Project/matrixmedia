//! LNBits Lightning Network payment provider.
//!
//! Implements `PaymentProvider` trait for Lightning payments via LNBits.
//! Supports instant donations in satoshis with near-zero fees.

pub mod client;
pub mod types;

use async_trait::async_trait;
use std::collections::HashMap;

use crate::provider::{
    CheckoutMode, CheckoutRequest, CheckoutResponse, OnboardingRequest, OnboardingResponse,
    PaymentError, PaymentProvider, WebhookEvent,
};
use client::LNBitsClient;
pub use types::{LNBitsWebhookPayload, LightningFeeBreakdown, calculate_lightning_fees};

/// LNBits payment provider for Lightning Network payments.
pub struct LNBitsProvider {
    client: LNBitsClient,
}

impl LNBitsProvider {
    pub fn new(
        base_url: &str,
        invoice_key: &str,
        admin_key: &str,
        webhook_url: Option<&str>,
    ) -> Self {
        Self {
            client: LNBitsClient::new(base_url, invoice_key, admin_key, webhook_url),
        }
    }

    /// Check if a specific payment has been settled.
    pub async fn check_payment(&self, payment_hash: &str) -> Result<bool, PaymentError> {
        let status = self.client.check_payment(payment_hash).await
            .map_err(|e| PaymentError::Internal(e))?;
        Ok(status.paid)
    }

    /// Get wallet balance in sats.
    pub async fn balance_sats(&self) -> Result<i64, PaymentError> {
        let info = self.client.wallet_info().await
            .map_err(|e| PaymentError::Internal(e))?;
        Ok(info.balance / 1000) // millisats to sats
    }
}

#[async_trait]
impl PaymentProvider for LNBitsProvider {
    fn name(&self) -> &str {
        "lightning"
    }

    async fn health_check(&self) -> Result<(), PaymentError> {
        self.client.health().await
            .map_err(|e| PaymentError::Unavailable(e))
    }

    /// Lightning doesn't need creator onboarding — anyone can receive.
    /// Returns a no-op response with the user_id as the "account".
    async fn create_connected_account(
        &self,
        req: OnboardingRequest,
    ) -> Result<OnboardingResponse, PaymentError> {
        // Lightning: no onboarding needed. The creator's wallet is their
        // Matrix identity. In production, creators could register their
        // own Lightning address (user@steegler.com) via LNURL.
        Ok(OnboardingResponse {
            account_id: format!("ln_{}", req.user_id.replace(['@', ':', '.'], "_")),
            onboarding_url: String::new(), // No onboarding page needed
        })
    }

    async fn check_onboarding_status(&self, _account_id: &str) -> Result<bool, PaymentError> {
        Ok(true) // Lightning: always "onboarded"
    }

    /// Create a Lightning invoice (bolt11) for the donation/subscription.
    ///
    /// Instead of a checkout URL (like Stripe), returns the bolt11 invoice
    /// string in the `checkout_url` field. The client shows a QR code.
    async fn create_checkout(
        &self,
        req: CheckoutRequest,
    ) -> Result<CheckoutResponse, PaymentError> {
        // Amount: for Lightning, amount_cents is treated as sats
        // (the client specifies amount in sats when provider=lightning)
        let amount_sats = req.amount_cents.unwrap_or(0);
        if amount_sats <= 0 {
            return Err(PaymentError::InvalidRequest("Amount must be positive".into()));
        }

        // Build memo from metadata
        let stream_id = req.metadata.get("stream_id").cloned().unwrap_or_default();
        let donor = req.metadata.get("donor_user_id").cloned().unwrap_or_default();
        let memo = format!("MatrixMedia donation: {} sats from {}", amount_sats, donor);

        // Extra metadata echoed back in webhook
        let mut extra = HashMap::new();
        for (k, v) in &req.metadata {
            extra.insert(k.clone(), v.clone());
        }
        extra.insert("creator_account_id".to_string(), req.creator_account_id.clone());
        extra.insert("amount_sats".to_string(), amount_sats.to_string());

        let invoice = self.client.create_invoice(amount_sats, &memo, Some(extra)).await
            .map_err(|e| PaymentError::PaymentFailed(e))?;

        Ok(CheckoutResponse {
            session_id: invoice.payment_hash.clone(),
            // For Lightning: checkout_url contains the bolt11 invoice
            // Client detects "lnbc" prefix → shows QR code instead of redirect
            checkout_url: invoice.payment_request,
        })
    }

    /// Verify a webhook from LNBits.
    /// LNBits doesn't sign webhooks — we verify by checking the payment_hash
    /// exists and is paid.
    async fn verify_webhook(
        &self,
        payload: &[u8],
        _signature: &str, // LNBits doesn't use signatures
    ) -> Result<WebhookEvent, PaymentError> {
        let webhook: LNBitsWebhookPayload = serde_json::from_slice(payload)
            .map_err(|e| PaymentError::WebhookInvalid(format!("Invalid JSON: {e}")))?;

        // Verify the payment actually exists and is paid
        let status = self.client.check_payment(&webhook.payment_hash).await
            .map_err(|e| PaymentError::Internal(e))?;

        if !status.paid {
            return Err(PaymentError::WebhookInvalid("Payment not settled".into()));
        }

        // Extract metadata
        let mut metadata = HashMap::new();
        for (k, v) in &webhook.extra {
            metadata.insert(k.clone(), v.to_string().trim_matches('"').to_string());
        }
        metadata.insert("payment_hash".to_string(), webhook.payment_hash.clone());
        metadata.insert("amount_sats".to_string(), webhook.amount.to_string());
        metadata.insert("provider".to_string(), "lightning".to_string());

        Ok(WebhookEvent::CheckoutCompleted {
            session_id: webhook.payment_hash,
            mode: CheckoutMode::Payment,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_name() {
        let provider = LNBitsProvider::new(
            "http://localhost:5000",
            "invoice_key",
            "admin_key",
            None,
        );
        assert_eq!(provider.name(), "lightning");
    }

    #[tokio::test]
    async fn test_onboarding_always_succeeds() {
        let provider = LNBitsProvider::new(
            "http://localhost:5000",
            "invoice_key",
            "admin_key",
            None,
        );
        let resp = provider.create_connected_account(OnboardingRequest {
            user_id: "@alice:example.com".into(),
            return_url: "http://localhost".into(),
            refresh_url: "http://localhost".into(),
        }).await.unwrap();
        assert!(resp.account_id.starts_with("ln_"));
        assert!(resp.onboarding_url.is_empty());
    }

    #[tokio::test]
    async fn test_onboarding_status_always_true() {
        let provider = LNBitsProvider::new(
            "http://localhost:5000",
            "invoice_key",
            "admin_key",
            None,
        );
        assert!(provider.check_onboarding_status("ln_alice").await.unwrap());
    }
}
