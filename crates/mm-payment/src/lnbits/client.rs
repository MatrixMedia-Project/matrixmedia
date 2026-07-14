//! LNBits HTTP API client.

use reqwest::Client;
use super::types::*;
use std::collections::HashMap;
use mm_core::http::SendTimed;

/// HTTP client for the LNBits API.
pub struct LNBitsClient {
    base_url: String,
    invoice_key: String,
    admin_key: String,
    webhook_url: Option<String>,
    http: Client,
}

impl LNBitsClient {
    pub fn new(
        base_url: &str,
        invoice_key: &str,
        admin_key: &str,
        webhook_url: Option<&str>,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            invoice_key: invoice_key.to_string(),
            admin_key: admin_key.to_string(),
            webhook_url: webhook_url.map(String::from),
            http: mm_core::http::shared().clone(),
        }
    }

    /// Get wallet info (balance, name).
    pub async fn wallet_info(&self) -> Result<WalletInfo, String> {
        let resp = self.http
            .get(format!("{}/api/v1/wallet", self.base_url))
            .header("X-Api-Key", &self.invoice_key)
            .send_timed(mm_core::http::DEP_LNBITS)
            .await
            .map_err(|e| format!("LNBits request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("LNBits wallet error {status}: {body}"));
        }

        resp.json::<WalletInfo>().await
            .map_err(|e| format!("LNBits parse error: {e}"))
    }

    /// Create a Lightning invoice (receive payment).
    pub async fn create_invoice(
        &self,
        amount_sats: i64,
        memo: &str,
        extra: Option<HashMap<String, String>>,
    ) -> Result<CreateInvoiceResponse, String> {
        let req = CreateInvoiceRequest {
            out: false, // incoming (receive)
            amount: amount_sats,
            memo: memo.to_string(),
            webhook: self.webhook_url.clone(),
            extra,
            expiry: Some(600), // 10 minutes
        };

        let resp = self.http
            .post(format!("{}/api/v1/payments", self.base_url))
            .header("X-Api-Key", &self.invoice_key)
            .json(&req)
            .send_timed(mm_core::http::DEP_LNBITS)
            .await
            .map_err(|e| format!("LNBits request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("LNBits invoice error {status}: {body}"));
        }

        resp.json::<CreateInvoiceResponse>().await
            .map_err(|e| format!("LNBits parse error: {e}"))
    }

    /// Check if a payment has been received.
    pub async fn check_payment(&self, payment_hash: &str) -> Result<PaymentStatus, String> {
        let resp = self.http
            .get(format!("{}/api/v1/payments/{}", self.base_url, payment_hash))
            .header("X-Api-Key", &self.invoice_key)
            .send_timed(mm_core::http::DEP_LNBITS)
            .await
            .map_err(|e| format!("LNBits request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("LNBits payment check error {status}: {body}"));
        }

        resp.json::<PaymentStatus>().await
            .map_err(|e| format!("LNBits parse error: {e}"))
    }

    /// Health check — can we reach LNBits?
    pub async fn health(&self) -> Result<(), String> {
        let _ = self.wallet_info().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = LNBitsClient::new(
            "http://localhost:5000",
            "invoice_key_hex",
            "admin_key_hex",
            Some("http://localhost/webhook"),
        );
        assert_eq!(client.base_url, "http://localhost:5000");
    }
}
