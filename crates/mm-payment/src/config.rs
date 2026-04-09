//! Payment engine configuration.
//!
//! Reads from the global [`mm_core::config::MonetizationConfig`] and provides
//! payment-specific defaults and validation.

use mm_core::config::MonetizationConfig;

/// Payment-specific configuration derived from MonetizationConfig.
#[derive(Debug, Clone)]
pub struct PaymentConfig {
    /// Whether the donation flow is active.
    pub donations_enabled: bool,
    /// Whether the subscription flow is active.
    pub subscriptions_enabled: bool,
    /// Minimum allowed donation in cents (e.g. 100 = $1.00).
    pub min_donation_cents: i64,
    /// Maximum allowed donation in cents.
    pub max_donation_cents: i64,
    /// Platform fee as a fraction (e.g. 0.10 = 10%).
    pub platform_fee_pct: f64,
    /// Stripe secret API key (`sk_live_...` or `sk_test_...`).
    pub stripe_secret_key: String,
    /// Stripe publishable key (client-side).
    pub stripe_publishable_key: String,
    /// Stripe webhook endpoint signing secret (`whsec_...`).
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
