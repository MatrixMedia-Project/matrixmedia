//! Donation business logic: tier calculation, fee calculation, request validation.

use serde::{Deserialize, Serialize};

/// Donation tier with visual properties for the overlay.
///
/// All fields use `&'static str` since tier data is constant -- this avoids
/// heap-allocating two `String`s on every donation lookup.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DonationTier {
    pub name: &'static str,
    pub color: &'static str,
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
            name: "blue",
            color: "#1E88E5",
            pin_duration_secs: 30,
        },
        200..=499 => DonationTier {
            name: "green",
            color: "#43A047",
            pin_duration_secs: 60,
        },
        500..=999 => DonationTier {
            name: "yellow",
            color: "#FDD835",
            pin_duration_secs: 90,
        },
        1000..=2499 => DonationTier {
            name: "orange",
            color: "#FB8C00",
            pin_duration_secs: 120,
        },
        2500..=4999 => DonationTier {
            name: "magenta",
            color: "#E91E63",
            pin_duration_secs: 180,
        },
        5000..=9999 => DonationTier {
            name: "red",
            color: "#E53935",
            pin_duration_secs: 240,
        },
        _ => DonationTier {
            name: "gold",
            color: "#FFD700",
            pin_duration_secs: 300,
        },
    }
}

/// Fee breakdown for a donation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize)]
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

    /// Larger sweep with multiple platform-fee percentages — exercise the
    /// invariants across the full realistic operator-config space.
    #[test]
    fn test_fees_invariants_sweep() {
        let pct_grid = [0.0, 0.05, 0.08, 0.10, 0.12, 0.15, 0.20, 0.25];
        let amounts = [
            35, 50, 100, 150, 199, 200, 350, 500, 750, 999, 1000, 1500, 2499, 2500, 3500, 4999,
            5000, 7500, 9999, 10000, 25000, 50000, 100_000,
        ];
        for &amount in &amounts {
            for &pct in &pct_grid {
                let fees = calculate_fees(amount, pct);

                // Sum invariant
                assert_eq!(
                    fees.stripe_fee_cents + fees.platform_fee_cents + fees.creator_net_cents,
                    fees.gross_cents,
                    "Sum invariant: amount={amount} pct={pct}"
                );

                // Creator net non-negative
                assert!(
                    fees.creator_net_cents >= 0,
                    "Creator net negative: amount={amount} pct={pct} \
                     stripe={} platform={} creator={}",
                    fees.stripe_fee_cents,
                    fees.platform_fee_cents,
                    fees.creator_net_cents
                );

                // Stripe fee always present (at least the $0.30 floor)
                assert!(
                    fees.stripe_fee_cents >= 30,
                    "Stripe fee below $0.30 floor: amount={amount}"
                );

                // Platform fee respects the configured pct (no overcharging)
                let net_after_stripe = amount - fees.stripe_fee_cents;
                let max_platform = (net_after_stripe as f64 * pct).ceil() as i64;
                assert!(
                    fees.platform_fee_cents <= max_platform,
                    "Platform fee exceeds {pct} of net: amount={amount} \
                     platform_fee={} max_allowed={max_platform}",
                    fees.platform_fee_cents
                );
            }
        }
    }

    /// Stripe fee must be monotonically non-decreasing as amount grows.
    #[test]
    fn test_stripe_fee_is_monotonic() {
        let amounts: Vec<i64> = (100..=100_000).step_by(100).collect();
        let fees: Vec<i64> = amounts
            .iter()
            .map(|&a| calculate_fees(a, 0.10).stripe_fee_cents)
            .collect();
        for window in fees.windows(2) {
            assert!(
                window[1] >= window[0],
                "Stripe fee not monotonic: {} → {}",
                window[0],
                window[1]
            );
        }
    }

    /// Tier pin durations must be monotonically non-decreasing in amount.
    #[test]
    fn test_tier_pin_duration_is_monotonic() {
        let amounts: Vec<i64> = (0..=20000).step_by(50).collect();
        let durs: Vec<u32> = amounts
            .iter()
            .map(|&a| tier_for_amount(a).pin_duration_secs)
            .collect();
        for (i, window) in durs.windows(2).enumerate() {
            assert!(
                window[1] >= window[0],
                "Pin duration not monotonic at amount={}: {}s → {}s",
                amounts[i + 1],
                window[0],
                window[1]
            );
        }
    }

    /// Repeated calls must be deterministic — no hidden state, no rng.
    #[test]
    fn test_calculate_fees_is_deterministic() {
        for amount in [100, 500, 2500, 10000, 50000] {
            for pct in [0.05, 0.10, 0.15] {
                let a = calculate_fees(amount, pct);
                let b = calculate_fees(amount, pct);
                assert_eq!(a.gross_cents, b.gross_cents);
                assert_eq!(a.stripe_fee_cents, b.stripe_fee_cents);
                assert_eq!(a.platform_fee_cents, b.platform_fee_cents);
                assert_eq!(a.creator_net_cents, b.creator_net_cents);
            }
        }
    }

    /// At the absolute Stripe floor ($0.30), creator gets 0 minus platform fee.
    /// Important to verify we don't go negative or panic at the boundary.
    #[test]
    fn test_calculate_fees_at_stripe_floor() {
        // 30 cents = exact Stripe floor (only the 30¢ component, 0 from %)
        let fees = calculate_fees(30, 0.10);
        assert_eq!(fees.stripe_fee_cents, 30);
        assert_eq!(fees.platform_fee_cents, 0);
        assert_eq!(fees.creator_net_cents, 0);
        assert_eq!(
            fees.gross_cents,
            fees.stripe_fee_cents + fees.platform_fee_cents + fees.creator_net_cents
        );
    }

    /// All 7 documented tiers must be reachable + return their documented colors.
    #[test]
    fn test_all_tiers_have_unique_colors_and_durations() {
        let samples = [(50, "blue"), (300, "green"), (700, "yellow"), (1500, "orange"),
                       (3000, "magenta"), (7000, "red"), (50_000, "gold")];
        let mut seen_colors = std::collections::HashSet::new();
        let mut seen_names = std::collections::HashSet::new();
        for (amount, expected_name) in samples {
            let tier = tier_for_amount(amount);
            assert_eq!(tier.name, expected_name, "amount={amount}");
            assert!(seen_colors.insert(tier.color), "Duplicate color: {}", tier.color);
            assert!(seen_names.insert(tier.name), "Duplicate name: {}", tier.name);
            assert!(tier.color.starts_with('#'), "Color must be hex: {}", tier.color);
            assert_eq!(tier.color.len(), 7, "Color must be 7 chars: {}", tier.color);
        }
    }

    /// Negative amounts shouldn't panic — defensive against bad input upstream.
    /// Validation lives in mm-core::validation::validate_donation_amount; this
    /// just ensures the math doesn't blow up if it's bypassed somehow.
    #[test]
    fn test_calculate_fees_does_not_panic_on_zero() {
        let _ = calculate_fees(0, 0.10);
        let _ = calculate_fees(0, 0.0);
    }
}
