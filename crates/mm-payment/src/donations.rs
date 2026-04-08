//! Donation business logic: tier calculation, fee calculation, request validation.

use serde::{Deserialize, Serialize};

/// Donation tier with visual properties for the overlay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DonationTier {
    pub name: String,
    pub color: String,
    pub pin_duration_secs: u32,
}

/// Map donation amount to visual tier.
///
/// | Amount  | Tier    | Color   | Pin Duration |
/// |---------|---------|---------|--------------|
/// | $1      | blue    | #1E88E5 | 30s          |
/// | $2      | green   | #43A047 | 60s          |
/// | $5      | yellow  | #FDD835 | 90s          |
/// | $10     | orange  | #FB8C00 | 120s         |
/// | $25     | magenta | #E91E63 | 180s         |
/// | $50     | red     | #E53935 | 240s         |
/// | $100+   | gold    | #FFD700 | 300s         |
pub fn tier_for_amount(amount_cents: i64) -> DonationTier {
    match amount_cents {
        0..=199 => DonationTier {
            name: "blue".into(),
            color: "#1E88E5".into(),
            pin_duration_secs: 30,
        },
        200..=499 => DonationTier {
            name: "green".into(),
            color: "#43A047".into(),
            pin_duration_secs: 60,
        },
        500..=999 => DonationTier {
            name: "yellow".into(),
            color: "#FDD835".into(),
            pin_duration_secs: 90,
        },
        1000..=2499 => DonationTier {
            name: "orange".into(),
            color: "#FB8C00".into(),
            pin_duration_secs: 120,
        },
        2500..=4999 => DonationTier {
            name: "magenta".into(),
            color: "#E91E63".into(),
            pin_duration_secs: 180,
        },
        5000..=9999 => DonationTier {
            name: "red".into(),
            color: "#E53935".into(),
            pin_duration_secs: 240,
        },
        _ => DonationTier {
            name: "gold".into(),
            color: "#FFD700".into(),
            pin_duration_secs: 300,
        },
    }
}

/// Fee breakdown for a donation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeBreakdown {
    pub gross_cents: i64,
    pub stripe_fee_cents: i64,
    pub platform_fee_cents: i64,
    pub creator_net_cents: i64,
}

/// Calculate fees for a donation amount.
///
/// Stripe fee: 2.9% + $0.30
/// Platform fee: `platform_fee_pct` of (gross - stripe_fee)
pub fn calculate_fees(amount_cents: i64, platform_fee_pct: f64) -> FeeBreakdown {
    let stripe_fee_cents = 30 + (amount_cents * 29 / 1000); // 2.9% + $0.30
    let net_after_stripe = amount_cents - stripe_fee_cents;
    let platform_fee_cents = (net_after_stripe as f64 * platform_fee_pct).round() as i64;
    let creator_net_cents = net_after_stripe - platform_fee_cents;

    FeeBreakdown {
        gross_cents: amount_cents,
        stripe_fee_cents,
        platform_fee_cents,
        creator_net_cents,
    }
}

/// Validated donation request (after input validation).
#[derive(Debug, Clone)]
pub struct DonationRequest {
    pub stream_id: String,
    pub donor_user_id: String,
    pub recipient_user_id: String,
    pub amount_cents: i64,
    pub currency: String,
    pub message: Option<String>,
    pub idempotency_key: String,
}

/// Result after donation checkout is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DonationResult {
    pub donation_id: String,
    pub checkout_url: String,
    pub tier: DonationTier,
    pub fees: FeeBreakdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_for_amount_all_tiers() {
        assert_eq!(tier_for_amount(100).name, "blue");
        assert_eq!(tier_for_amount(199).name, "blue");
        assert_eq!(tier_for_amount(200).name, "green");
        assert_eq!(tier_for_amount(500).name, "yellow");
        assert_eq!(tier_for_amount(1000).name, "orange");
        assert_eq!(tier_for_amount(2500).name, "magenta");
        assert_eq!(tier_for_amount(5000).name, "red");
        assert_eq!(tier_for_amount(10000).name, "gold");
        assert_eq!(tier_for_amount(50000).name, "gold");
    }

    #[test]
    fn test_tier_pin_durations() {
        assert_eq!(tier_for_amount(100).pin_duration_secs, 30);
        assert_eq!(tier_for_amount(500).pin_duration_secs, 90);
        assert_eq!(tier_for_amount(10000).pin_duration_secs, 300);
    }

    #[test]
    fn test_calculate_fees_one_dollar() {
        let fees = calculate_fees(100, 0.10);
        assert_eq!(fees.gross_cents, 100);
        assert_eq!(fees.stripe_fee_cents, 32); // 30 + (100 * 29 / 1000) = 30 + 2
        assert!(fees.creator_net_cents > 0);
        assert_eq!(
            fees.gross_cents,
            fees.stripe_fee_cents + fees.platform_fee_cents + fees.creator_net_cents
        );
    }

    #[test]
    fn test_calculate_fees_hundred_dollars() {
        let fees = calculate_fees(10000, 0.10);
        assert_eq!(fees.gross_cents, 10000);
        assert_eq!(fees.stripe_fee_cents, 320); // 30 + 290
        let net = 10000 - 320; // 9680
        assert_eq!(fees.platform_fee_cents, 968); // 9680 * 0.10
        assert_eq!(fees.creator_net_cents, net - 968);
    }

    #[test]
    fn test_calculate_fees_zero_platform_fee() {
        let fees = calculate_fees(500, 0.0);
        assert_eq!(fees.platform_fee_cents, 0);
        assert_eq!(fees.creator_net_cents, 500 - fees.stripe_fee_cents);
    }

    #[test]
    fn test_fee_components_sum_to_gross() {
        for amount in [100, 200, 500, 1000, 2500, 5000, 10000] {
            let fees = calculate_fees(amount, 0.10);
            assert_eq!(
                fees.stripe_fee_cents + fees.platform_fee_cents + fees.creator_net_cents,
                fees.gross_cents,
                "Fee components don't sum to gross for amount {amount}"
            );
        }
    }

    #[test]
    fn test_tier_boundary_values() {
        // Exact boundaries between each tier
        assert_eq!(tier_for_amount(199).name, "blue");
        assert_eq!(tier_for_amount(200).name, "green");
        assert_eq!(tier_for_amount(499).name, "green");
        assert_eq!(tier_for_amount(500).name, "yellow");
        assert_eq!(tier_for_amount(999).name, "yellow");
        assert_eq!(tier_for_amount(1000).name, "orange");
        assert_eq!(tier_for_amount(2499).name, "orange");
        assert_eq!(tier_for_amount(2500).name, "magenta");
        assert_eq!(tier_for_amount(4999).name, "magenta");
        assert_eq!(tier_for_amount(5000).name, "red");
        assert_eq!(tier_for_amount(9999).name, "red");
        assert_eq!(tier_for_amount(10000).name, "gold");
        // Zero and negative
        assert_eq!(tier_for_amount(0).name, "blue");
    }

    #[test]
    fn test_fees_sum_invariant_all_amounts() {
        // For amounts 100..=10000 step 100, verify the invariant:
        // stripe_fee + platform_fee + creator_net == gross
        let mut amount = 100;
        while amount <= 10000 {
            let fees = calculate_fees(amount, 0.10);
            assert_eq!(
                fees.stripe_fee_cents + fees.platform_fee_cents + fees.creator_net_cents,
                fees.gross_cents,
                "Invariant violation at amount={amount}"
            );
            assert!(
                fees.creator_net_cents >= 0,
                "Creator net negative at amount={amount}"
            );
            amount += 100;
        }
    }
}
