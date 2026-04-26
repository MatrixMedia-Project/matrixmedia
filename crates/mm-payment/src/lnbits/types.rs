//! LNBits API request/response types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Request to create a Lightning invoice (receive payment).
#[derive(Debug, Serialize)]
pub struct CreateInvoiceRequest {
    /// false = receive (incoming invoice), true = pay (outgoing)
    pub out: bool,
    /// Amount in satoshis
    pub amount: i64,
    /// Human-readable memo
    pub memo: String,
    /// Webhook URL to call when payment is received
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    /// Extra metadata (echoed back in webhook)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, String>>,
    /// Invoice expiry in seconds (default: 600 = 10 min)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<i64>,
}

/// Response from creating an invoice.
#[derive(Debug, Deserialize)]
pub struct CreateInvoiceResponse {
    /// Unique payment identifier
    pub payment_hash: String,
    /// BOLT11 invoice string (starts with lnbc...)
    pub payment_request: String,
    /// Checking ID for status queries
    #[serde(default)]
    pub checking_id: String,
}

/// Payment status response.
#[derive(Debug, Deserialize)]
pub struct PaymentStatus {
    pub paid: bool,
    #[serde(default)]
    pub amount: i64,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub time: i64,
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Wallet info response.
#[derive(Debug, Deserialize)]
pub struct WalletInfo {
    pub id: String,
    pub name: String,
    pub balance: i64, // in millisats (divide by 1000 for sats)
}

/// Webhook payload from LNBits when payment is received.
#[derive(Debug, Deserialize)]
pub struct LNBitsWebhookPayload {
    pub payment_hash: String,
    #[serde(default)]
    pub payment_request: String,
    #[serde(default)]
    pub amount: i64,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Lightning fee breakdown.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LightningFeeBreakdown {
    pub gross_sats: i64,
    pub network_fee_sats: i64,
    pub platform_fee_sats: i64,
    pub creator_net_sats: i64,
    /// Approximate USD cents (for tier matching)
    pub approx_usd_cents: i64,
}

/// Convert sats to approximate USD cents.
/// Uses a fixed rate for simplicity. In production, fetch from an exchange API.
pub fn sats_to_usd_cents(sats: i64) -> i64 {
    // Approximate: 1 USD ≈ 1500 sats (adjust as BTC price changes)
    // 1 sat ≈ 0.067 cents
    (sats as f64 * 100.0 / 1500.0).round() as i64
}

/// Convert USD cents to approximate sats.
pub fn usd_cents_to_sats(cents: i64) -> i64 {
    (cents as f64 * 1500.0 / 100.0).round() as i64
}

/// Calculate fees for a Lightning donation.
pub fn calculate_lightning_fees(amount_sats: i64, platform_fee_pct: f64) -> LightningFeeBreakdown {
    let network_fee_sats = 1.max(amount_sats / 1000); // ~0.1% or 1 sat minimum
    let net_after_network = amount_sats - network_fee_sats;
    let platform_fee_sats = (net_after_network as f64 * platform_fee_pct).round() as i64;
    let creator_net_sats = net_after_network - platform_fee_sats;

    LightningFeeBreakdown {
        gross_sats: amount_sats,
        network_fee_sats,
        platform_fee_sats,
        creator_net_sats,
        approx_usd_cents: sats_to_usd_cents(amount_sats),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sats_to_usd() {
        assert_eq!(sats_to_usd_cents(1500), 100); // 1500 sats ≈ $1.00
        assert_eq!(sats_to_usd_cents(15000), 1000); // 15000 sats ≈ $10.00
    }

    #[test]
    fn test_usd_to_sats() {
        assert_eq!(usd_cents_to_sats(100), 1500); // $1.00 ≈ 1500 sats
    }

    #[test]
    fn test_lightning_fees() {
        let fees = calculate_lightning_fees(10000, 0.10);
        assert_eq!(fees.gross_sats, 10000);
        assert!(fees.network_fee_sats >= 1);
        assert!(fees.platform_fee_sats > 0);
        assert!(fees.creator_net_sats > 0);
        assert_eq!(
            fees.network_fee_sats + fees.platform_fee_sats + fees.creator_net_sats,
            fees.gross_sats
        );
    }

    /// Round-trip: cents → sats → cents must stay in the same vicinity.
    /// Pinned exchange rate is 1500 sats/USD = 15 sats/cent. So a clean
    /// round-trip on cents that are multiples of (15 sats / 1 cent) base
    /// must be exact; rounding may drift a single cent otherwise.
    #[test]
    fn test_cents_sats_roundtrip_is_close() {
        for cents in [100, 200, 500, 1000, 2500, 5000, 10000, 50000] {
            let sats = usd_cents_to_sats(cents);
            let back = sats_to_usd_cents(sats);
            let drift = (back - cents).abs();
            assert!(
                drift <= 1,
                "Round-trip drift > 1 cent: {} → {} sats → {} (drift={})",
                cents,
                sats,
                back,
                drift
            );
        }
    }

    /// Conversions must be monotonic in both directions.
    #[test]
    fn test_conversions_are_monotonic() {
        let mut last_sats = 0;
        let mut last_cents = 0;
        for cents in (0..=10_000).step_by(50) {
            let sats = usd_cents_to_sats(cents);
            assert!(sats >= last_sats, "usd_cents_to_sats not monotonic at {cents}");
            last_sats = sats;
        }
        for sats in (0..=150_000).step_by(500) {
            let cents = sats_to_usd_cents(sats);
            assert!(cents >= last_cents, "sats_to_usd_cents not monotonic at {sats}");
            last_cents = cents;
        }
    }

    /// Lightning fee invariants across the realistic operator-config space.
    #[test]
    fn test_lightning_fees_invariants_sweep() {
        let pct_grid = [0.0, 0.05, 0.08, 0.10, 0.12, 0.15, 0.20];
        let amounts = [
            100, 500, 1000, 1500, 5000, 10000, 25000, 50000, 100_000, 500_000, 1_000_000,
        ];
        for &amount in &amounts {
            for &pct in &pct_grid {
                let fees = calculate_lightning_fees(amount, pct);

                // Sum invariant
                assert_eq!(
                    fees.network_fee_sats + fees.platform_fee_sats + fees.creator_net_sats,
                    fees.gross_sats,
                    "Sum invariant: amount={amount} sats pct={pct}"
                );

                // Creator never negative
                assert!(
                    fees.creator_net_sats >= 0,
                    "Creator net negative: amount={amount} sats pct={pct}"
                );

                // Network fee non-zero (Lightning routing always costs at least 1 sat)
                assert!(
                    fees.network_fee_sats >= 1,
                    "Network fee below 1 sat floor: amount={amount}"
                );
            }
        }
    }

    /// Determinism check.
    #[test]
    fn test_lightning_fees_deterministic() {
        for amount in [1000, 10000, 100_000] {
            for pct in [0.05, 0.10, 0.15] {
                let a = calculate_lightning_fees(amount, pct);
                let b = calculate_lightning_fees(amount, pct);
                assert_eq!(a.gross_sats, b.gross_sats);
                assert_eq!(a.network_fee_sats, b.network_fee_sats);
                assert_eq!(a.platform_fee_sats, b.platform_fee_sats);
                assert_eq!(a.creator_net_sats, b.creator_net_sats);
            }
        }
    }

    /// Approx USD cents in the breakdown matches the standalone conversion.
    #[test]
    fn test_lightning_fees_approx_usd_matches_standalone() {
        for sats in [1_500, 15_000, 150_000] {
            let fees = calculate_lightning_fees(sats, 0.10);
            assert_eq!(fees.approx_usd_cents, sats_to_usd_cents(sats));
        }
    }

    /// Zero is safe; no panic.
    #[test]
    fn test_lightning_fees_does_not_panic_on_zero() {
        let _ = calculate_lightning_fees(0, 0.10);
        let _ = usd_cents_to_sats(0);
        let _ = sats_to_usd_cents(0);
    }
}
