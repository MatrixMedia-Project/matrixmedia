//! Subscription business logic: tier validation, state transitions, fee calculation.

use serde::{Deserialize, Serialize};

/// Maximum number of tiers a creator can have.
pub const MAX_TIERS_PER_CREATOR: usize = 5;

/// Valid tier level range.
pub const MIN_TIER_LEVEL: i32 = 1;
pub const MAX_TIER_LEVEL: i32 = 5;

/// Price bounds in cents.
pub const MIN_PRICE_CENTS: i64 = 99;
pub const MAX_PRICE_CENTS: i64 = 4999;

/// Subscription status values (stored as lowercase strings in DB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubscriptionStatus {
    Active,
    Canceled,
    PastDue,
    Incomplete,
}

impl SubscriptionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Canceled => "canceled",
            Self::PastDue => "past_due",
            Self::Incomplete => "incomplete",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "canceled" => Self::Canceled,
            "past_due" => Self::PastDue,
            "incomplete" => Self::Incomplete,
            _ => Self::Incomplete,
        }
    }
}

/// Validate a tier-creation request.
pub fn validate_tier(
    tier_level: i32,
    price_cents: i64,
    existing_tier_count: usize,
) -> Result<(), String> {
    if !(MIN_TIER_LEVEL..=MAX_TIER_LEVEL).contains(&tier_level) {
        return Err(format!(
            "tier_level must be between {MIN_TIER_LEVEL} and {MAX_TIER_LEVEL}"
        ));
    }
    if !(MIN_PRICE_CENTS..=MAX_PRICE_CENTS).contains(&price_cents) {
        return Err(format!(
            "price must be between {MIN_PRICE_CENTS} and {MAX_PRICE_CENTS} cents"
        ));
    }
    if existing_tier_count >= MAX_TIERS_PER_CREATOR {
        return Err(format!(
            "maximum of {MAX_TIERS_PER_CREATOR} tiers per creator"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscription_status_roundtrip() {
        let variants = [
            (SubscriptionStatus::Active, "active"),
            (SubscriptionStatus::Canceled, "canceled"),
            (SubscriptionStatus::PastDue, "past_due"),
            (SubscriptionStatus::Incomplete, "incomplete"),
        ];
        for (variant, expected_str) in &variants {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(SubscriptionStatus::from_str(expected_str), *variant);
        }
    }

    #[test]
    fn test_subscription_status_unknown_defaults() {
        assert_eq!(
            SubscriptionStatus::from_str("unknown"),
            SubscriptionStatus::Incomplete
        );
    }

    #[test]
    fn test_validate_tier_happy_path() {
        assert!(validate_tier(1, 499, 0).is_ok());
        assert!(validate_tier(5, 4999, 4).is_ok());
    }

    #[test]
    fn test_validate_tier_level_bounds() {
        assert!(validate_tier(0, 499, 0).is_err());
        assert!(validate_tier(6, 499, 0).is_err());
    }

    #[test]
    fn test_validate_tier_price_bounds() {
        assert!(validate_tier(1, 98, 0).is_err());
        assert!(validate_tier(1, 5000, 0).is_err());
        assert!(validate_tier(1, 99, 0).is_ok());
        assert!(validate_tier(1, 4999, 0).is_ok());
    }

    #[test]
    fn test_validate_tier_max_count() {
        assert!(validate_tier(1, 499, 5).is_err());
        assert!(validate_tier(1, 499, 4).is_ok());
    }
}
