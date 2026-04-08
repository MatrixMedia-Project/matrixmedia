//! Payment engine configuration.
//!
//! Reads from the global [`mm_core::config::MonetizationConfig`] and provides
//! payment-specific defaults and validation.

use mm_core::config::MonetizationConfig;

/// Payment-specific configuration derived from MonetizationConfig.
#[derive(Debug, Clone)]
pub struct PaymentConfig {
    pub donations_enabled: bool,
    pub subscriptions_enabled: bool,
    pub min_donation_cents: i64,
    pub max_donation_cents: i64,
    pub platform_fee_pct: f64,
    pub stripe_secret_key: String,
    pub stripe_publishable_key: String,
    pub webhook_signing_secret: String,
}

impl From<&MonetizationConfig> for PaymentConfig {
    fn from(mc: &MonetizationConfig) -> Self {
        Self {
            donations_enabled: mc.donations_enabled,
            subscriptions_enabled: mc.subscriptions_enabled,
            min_donation_cents: mc.min_donation_cents,
            max_donation_cents: mc.max_donation_cents,
            platform_fee_pct: mc.platform_fee_pct,
            stripe_secret_key: mc.stripe_secret_key.clone(),
            stripe_publishable_key: mc.stripe_publishable_key.clone(),
            webhook_signing_secret: mc.webhook_signing_secret.clone(),
        }
    }
}
