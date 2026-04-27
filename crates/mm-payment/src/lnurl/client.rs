//! HTTP client that resolves a Lightning Address into a fresh BOLT11 invoice.

use std::time::Duration;

use reqwest::Client;
use url::Url;

use super::types::{
    LightningAddress, LnurlError, LnurlErrorPayload, LnurlPayInvoice, LnurlPayMetadata,
    parse_lightning_address,
};

/// Default timeout for both the metadata fetch and the invoice request.
///
/// LNURL endpoints are usually quick (well under 1s) but some self-hosted
/// nodes can be slow when the incoming-channel router needs to find a route.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Maximum response body we'll buffer from a Lightning Address endpoint.
/// LUD-06 documents are tiny (~500B); 16 KiB is generous.
const MAX_BODY_BYTES: u64 = 16 * 1024;

/// Resolves Lightning Addresses (LUD-16) into BOLT11 invoices via LNURL-pay
/// (LUD-06). Stateless and cheap to clone — wraps a shared `reqwest::Client`.
#[derive(Debug, Clone)]
pub struct LnurlPayClient {
    http: Client,
}

impl Default for LnurlPayClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LnurlPayClient {
    pub fn new() -> Self {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("mm-payment/", env!("CARGO_PKG_VERSION")))
            // LUD-06 is HTTPS-only; rustls is wired through reqwest's default features.
            .build()
            .expect("reqwest client builds with default config");
        Self { http }
    }

    /// Construct from a pre-configured client (lets callers inject middleware
    /// or override the default timeout). Mostly used in tests.
    pub fn with_client(http: Client) -> Self {
        Self { http }
    }

    /// One-shot helper: parse → fetch metadata → request invoice.
    ///
    /// `amount_msat` is the amount to charge in **millisatoshis**. The
    /// recipient's metadata `min_sendable` / `max_sendable` are enforced
    /// before we even hit the callback URL.
    pub async fn request_invoice(
        &self,
        address: &str,
        amount_msat: u64,
        comment: Option<&str>,
    ) -> Result<LnurlPayInvoice, LnurlError> {
        let parsed = parse_lightning_address(address)?;
        let metadata = self.fetch_metadata(&parsed).await?;
        self.request_invoice_with_metadata(&metadata, amount_msat, comment)
            .await
    }

    /// Step 1 — `GET https://{domain}/.well-known/lnurlp/{name}`.
    pub async fn fetch_metadata(
        &self,
        address: &LightningAddress,
    ) -> Result<LnurlPayMetadata, LnurlError> {
        let url = address.well_known_url();
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| LnurlError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LnurlError::Remote(format!(
                "metadata GET {} returned HTTP {}",
                url,
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LnurlError::Network(e.to_string()))?;
        if (bytes.len() as u64) > MAX_BODY_BYTES {
            return Err(LnurlError::InvalidMetadata(format!(
                "metadata body too large ({} bytes)",
                bytes.len()
            )));
        }

        let metadata: LnurlPayMetadata = serde_json::from_slice(&bytes)
            .map_err(|e| LnurlError::InvalidMetadata(e.to_string()))?;
        metadata.validate()?;
        Ok(metadata)
    }

    /// Step 2 — `GET {callback}?amount={msat}[&comment={c}]`.
    ///
    /// Splits the call from `fetch_metadata` so callers can validate, log,
    /// or cache the metadata document independently of invoice generation.
    pub async fn request_invoice_with_metadata(
        &self,
        metadata: &LnurlPayMetadata,
        amount_msat: u64,
        comment: Option<&str>,
    ) -> Result<LnurlPayInvoice, LnurlError> {
        if amount_msat < metadata.min_sendable || amount_msat > metadata.max_sendable {
            return Err(LnurlError::AmountOutOfRange {
                amount_msat,
                min_msat: metadata.min_sendable,
                max_msat: metadata.max_sendable,
            });
        }

        let comment = match comment {
            Some(c) if !c.is_empty() => {
                if metadata.comment_allowed == 0 {
                    // Recipient doesn't accept comments; drop silently rather
                    // than 4xx — LNURL clients commonly do this.
                    None
                } else if c.len() > metadata.comment_allowed as usize {
                    return Err(LnurlError::CommentTooLong {
                        len: c.len(),
                        max_allowed: metadata.comment_allowed as usize,
                    });
                } else {
                    Some(c.to_owned())
                }
            }
            _ => None,
        };

        let mut url = Url::parse(&metadata.callback)
            .map_err(|e| LnurlError::InvalidMetadata(format!("bad callback URL: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("amount", &amount_msat.to_string());
            if let Some(c) = comment.as_deref() {
                q.append_pair("comment", c);
            }
        }

        let resp = self
            .http
            .get(url.as_str())
            .send()
            .await
            .map_err(|e| LnurlError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(LnurlError::Remote(format!(
                "callback returned HTTP {}",
                resp.status()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| LnurlError::Network(e.to_string()))?;
        if (bytes.len() as u64) > MAX_BODY_BYTES {
            return Err(LnurlError::Remote(format!(
                "invoice body too large ({} bytes)",
                bytes.len()
            )));
        }

        if let Ok(err) = serde_json::from_slice::<LnurlErrorPayload>(&bytes) {
            if err.status.eq_ignore_ascii_case("ERROR") {
                return Err(LnurlError::Remote(err.reason));
            }
        }

        let invoice: LnurlPayInvoice = serde_json::from_slice(&bytes)
            .map_err(|e| LnurlError::Remote(format!("invoice payload malformed: {e}")))?;

        if !invoice.pr.starts_with("ln") {
            return Err(LnurlError::Remote(format!(
                "invoice payload missing BOLT11: {}",
                invoice.pr
            )));
        }

        Ok(invoice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lnurl::types::LnurlPayMetadata;

    fn meta(min: u64, max: u64, comment: u32) -> LnurlPayMetadata {
        LnurlPayMetadata {
            callback: "https://example.com/cb?creator=42".into(),
            max_sendable: max,
            min_sendable: min,
            metadata: "[]".into(),
            tag: "payRequest".into(),
            comment_allowed: comment,
        }
    }

    #[test]
    fn client_constructs() {
        let _ = LnurlPayClient::new();
        let _ = LnurlPayClient::default();
    }

    #[tokio::test]
    async fn rejects_amount_below_min() {
        let client = LnurlPayClient::new();
        let m = meta(1_000, 100_000_000, 0);
        let err = client
            .request_invoice_with_metadata(&m, 500, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LnurlError::AmountOutOfRange { .. }));
    }

    #[tokio::test]
    async fn rejects_amount_above_max() {
        let client = LnurlPayClient::new();
        let m = meta(1_000, 100_000_000, 0);
        let err = client
            .request_invoice_with_metadata(&m, 100_000_001, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LnurlError::AmountOutOfRange { .. }));
    }

    #[tokio::test]
    async fn rejects_comment_too_long() {
        let client = LnurlPayClient::new();
        let m = meta(1_000, 100_000_000, 8);
        let err = client
            .request_invoice_with_metadata(&m, 5_000, Some("this is way too long"))
            .await
            .unwrap_err();
        assert!(matches!(err, LnurlError::CommentTooLong { .. }));
    }

    #[tokio::test]
    async fn rejects_bad_callback_url() {
        let client = LnurlPayClient::new();
        let mut m = meta(1_000, 100_000_000, 0);
        m.callback = "not a url".into();
        let err = client
            .request_invoice_with_metadata(&m, 5_000, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LnurlError::InvalidMetadata(_)));
    }

    #[test]
    fn well_known_url_for_parsed_address() {
        let addr = parse_lightning_address("alice@phoenix.acinq.co").unwrap();
        assert_eq!(
            addr.well_known_url(),
            "https://phoenix.acinq.co/.well-known/lnurlp/alice"
        );
    }
}
