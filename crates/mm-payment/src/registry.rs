//! Payment provider registry -- manages multiple active providers.
//!
//! Phase 7a: Single provider (Stripe).
//! Phase 8: Multiple providers simultaneously (Stripe + PayPal + crypto).

use std::collections::HashMap;
use std::sync::Arc;

use crate::provider::{
    CheckoutRequest, CheckoutResponse, OnboardingRequest, OnboardingResponse, PaymentError,
    PaymentProvider, WebhookEvent,
};

/// Registry of active payment providers.
///
/// Operators can enable multiple providers simultaneously.
/// Viewers choose which provider to pay with at checkout.
pub struct PaymentProviderRegistry {
    providers: HashMap<String, Arc<dyn PaymentProvider>>,
}

impl PaymentProviderRegistry {
    /// Create a new registry with no providers.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider.
    pub fn register(&mut self, provider: Arc<dyn PaymentProvider>) {
        let name = provider.name().to_string();
        self.providers.insert(name, provider);
    }

    /// List all registered provider names.
    pub fn available_providers(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<&dyn PaymentProvider> {
        self.providers.get(name).map(|p| p.as_ref())
    }

    /// Create checkout via a specific provider.
    pub async fn create_checkout(
        &self,
        provider: &str,
        req: CheckoutRequest,
    ) -> Result<CheckoutResponse, PaymentError> {
        let p = self.providers.get(provider).ok_or_else(|| {
            PaymentError::NotFound(format!("Provider '{provider}' not registered"))
        })?;
        p.create_checkout(req).await
    }

    /// Onboard creator via a specific provider.
    pub async fn onboard(
        &self,
        provider: &str,
        req: OnboardingRequest,
    ) -> Result<OnboardingResponse, PaymentError> {
        let p = self.providers.get(provider).ok_or_else(|| {
            PaymentError::NotFound(format!("Provider '{provider}' not registered"))
        })?;
        p.create_connected_account(req).await
    }

    /// Verify webhook from a specific provider.
    pub async fn verify_webhook(
        &self,
        provider: &str,
        payload: &[u8],
        signature: &str,
    ) -> Result<WebhookEvent, PaymentError> {
        let p = self.providers.get(provider).ok_or_else(|| {
            PaymentError::NotFound(format!("Provider '{provider}' not registered"))
        })?;
        p.verify_webhook(payload, signature).await
    }

    /// Health check all providers.
    pub async fn health_check_all(&self) -> HashMap<String, Result<(), PaymentError>> {
        let mut results = HashMap::new();
        for (name, provider) in &self.providers {
            results.insert(name.clone(), provider.health_check().await);
        }
        results
    }
}

impl Default for PaymentProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;

    #[test]
    fn test_registry_empty() {
        let registry = PaymentProviderRegistry::new();
        assert!(registry.available_providers().is_empty());
        assert!(registry.get("stripe").is_none());
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = PaymentProviderRegistry::new();
        registry.register(Arc::new(MockProvider::new()));
        assert!(registry.get("mock").is_some());
        assert_eq!(registry.get("mock").unwrap().name(), "mock");
    }

    #[test]
    fn test_registry_unknown_provider_returns_none() {
        let registry = PaymentProviderRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_registry_unknown_provider_checkout_returns_error() {
        let registry = PaymentProviderRegistry::new();
        let result = registry
            .create_checkout(
                "nonexistent",
                crate::provider::CheckoutRequest {
                    mode: crate::provider::CheckoutMode::Payment,
                    amount_cents: Some(500),
                    currency: "usd".into(),
                    creator_account_id: "acct_123".into(),
                    platform_fee_cents: Some(50),
                    success_url: "http://localhost/ok".into(),
                    cancel_url: "http://localhost/cancel".into(),
                    metadata: std::collections::HashMap::new(),
                    price_id: None,
                },
            )
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, PaymentError::NotFound(_)));
    }

    #[test]
    fn test_registry_list_available() {
        let mut registry = PaymentProviderRegistry::new();
        registry.register(Arc::new(MockProvider::new()));
        let providers = registry.available_providers();
        assert_eq!(providers.len(), 1);
        assert!(providers.contains(&"mock"));
    }
}
