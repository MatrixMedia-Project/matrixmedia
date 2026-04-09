//! Mock payment provider for testing.
//!
//! Always succeeds. Returns predictable responses.

use crate::provider::{
    CheckoutMode, CheckoutRequest, CheckoutResponse, OnboardingRequest, OnboardingResponse,
    PaymentError, PaymentProvider, WebhookEvent,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Mock payment provider that always succeeds.
pub struct MockProvider {
    counter: AtomicU64,
    name: String,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            name: "mock".to_string(),
        }
    }

    /// Create a MockProvider that registers under a custom name.
    /// Use `with_name("stripe")` to substitute for Stripe in dev.
    pub fn with_name(name: &str) -> Self {
        Self {
            counter: AtomicU64::new(0),
            name: name.to_string(),
        }
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("mock_{n}")
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PaymentProvider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn health_check(&self) -> Result<(), PaymentError> {
        Ok(())
    }

    async fn create_connected_account(
        &self,
        req: OnboardingRequest,
    ) -> Result<OnboardingResponse, PaymentError> {
        Ok(OnboardingResponse {
            account_id: self.next_id(),
            onboarding_url: format!("https://mock.example.com/onboard?user={}", req.user_id),
        })
    }

    async fn check_onboarding_status(&self, _account_id: &str) -> Result<bool, PaymentError> {
        Ok(true) // Always onboarded
    }

    async fn create_checkout(
        &self,
        req: CheckoutRequest,
    ) -> Result<CheckoutResponse, PaymentError> {
        let session_id = self.next_id();
        Ok(CheckoutResponse {
            session_id: session_id.clone(),
            checkout_url: format!(
                "https://mock.example.com/checkout/{session_id}?amount={}&to={}",
                req.amount_cents.unwrap_or(0),
                req.creator_account_id
            ),
        })
    }

    async fn verify_webhook(
        &self,
        _payload: &[u8],
        _signature: &str,
    ) -> Result<WebhookEvent, PaymentError> {
        // Mock: always return a checkout completed event
        Ok(WebhookEvent::CheckoutCompleted {
            session_id: self.next_id(),
            mode: CheckoutMode::Payment,
            metadata: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider_health() {
        let mock = MockProvider::new();
        assert!(mock.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn test_mock_provider_onboarding() {
        let mock = MockProvider::new();
        let resp = mock
            .create_connected_account(OnboardingRequest {
                user_id: "@alice:example.com".into(),
                return_url: "http://localhost".into(),
                refresh_url: "http://localhost".into(),
            })
            .await
            .unwrap();
        assert!(!resp.account_id.is_empty());
        assert!(resp.onboarding_url.contains("alice"));
    }

    #[tokio::test]
    async fn test_mock_provider_checkout() {
        let mock = MockProvider::new();
        let resp = mock
            .create_checkout(CheckoutRequest {
                mode: CheckoutMode::Payment,
                amount_cents: Some(500),
                currency: "usd".into(),
                creator_account_id: "acct_123".into(),
                platform_fee_cents: Some(50),
                success_url: "http://localhost/success".into(),
                cancel_url: "http://localhost/cancel".into(),
                metadata: HashMap::new(),
                price_id: None,
            })
            .await
            .unwrap();
        assert!(!resp.checkout_url.is_empty());
        assert!(resp.checkout_url.contains("500"));
    }

    #[tokio::test]
    async fn test_mock_provider_name() {
        let mock = MockProvider::new();
        assert_eq!(mock.name(), "mock");
    }
}
