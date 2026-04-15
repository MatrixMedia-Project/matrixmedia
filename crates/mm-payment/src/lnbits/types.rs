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
}
