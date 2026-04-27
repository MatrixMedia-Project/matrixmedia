//! LNURL-pay protocol types (LUD-06) and Lightning Address parsing (LUD-16).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised by the LNURL-pay client.
#[derive(Debug, Error)]
pub enum LnurlError {
    #[error("invalid Lightning Address: {0}")]
    InvalidAddress(String),

    #[error("amount {amount_msat} msat is outside allowed range [{min_msat}, {max_msat}]")]
    AmountOutOfRange {
        amount_msat: u64,
        min_msat: u64,
        max_msat: u64,
    },

    #[error("comment too long ({len} > {max_allowed})")]
    CommentTooLong { len: usize, max_allowed: usize },

    #[error("LNURL-pay metadata document is malformed: {0}")]
    InvalidMetadata(String),

    #[error("LNURL-pay endpoint returned an error: {0}")]
    Remote(String),

    #[error("network error: {0}")]
    Network(String),
}

/// LNURL-pay metadata document (LUD-06 §"GET LNURL-pay request").
///
/// Returned by the `.well-known/lnurlp/{name}` endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LnurlPayMetadata {
    /// URL the client must call to fetch an actual invoice.
    pub callback: String,

    /// Maximum sendable amount in **millisatoshis**.
    #[serde(rename = "maxSendable")]
    pub max_sendable: u64,

    /// Minimum sendable amount in **millisatoshis**.
    #[serde(rename = "minSendable")]
    pub min_sendable: u64,

    /// JSON-encoded metadata array; payer wallets display this.
    #[serde(default)]
    pub metadata: String,

    /// Should always be `"payRequest"` for LUD-06.
    #[serde(default)]
    pub tag: String,

    /// Maximum comment length the recipient accepts (LUD-12). Zero / absent
    /// means comments are not supported.
    #[serde(rename = "commentAllowed", default)]
    pub comment_allowed: u32,
}

impl LnurlPayMetadata {
    /// Surface-level sanity check: tag must be `payRequest` and the bounds
    /// must be coherent. We do **not** parse the `metadata` field — wallets
    /// own that — but we refuse documents that are obviously not LUD-06.
    pub fn validate(&self) -> Result<(), LnurlError> {
        if self.tag != "payRequest" {
            return Err(LnurlError::InvalidMetadata(format!(
                "expected tag=payRequest, got {:?}",
                self.tag
            )));
        }
        if self.min_sendable == 0 || self.max_sendable < self.min_sendable {
            return Err(LnurlError::InvalidMetadata(format!(
                "incoherent sendable bounds: min={} max={}",
                self.min_sendable, self.max_sendable
            )));
        }
        Ok(())
    }
}

/// Response from a callback URL — the actual BOLT11 invoice (LUD-06 §"Invoice").
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LnurlPayInvoice {
    /// BOLT11 payment request (`lnbc...`).
    pub pr: String,

    /// Optional success action shown to the payer after payment settles.
    #[serde(rename = "successAction", default, skip_serializing_if = "Option::is_none")]
    pub success_action: Option<serde_json::Value>,

    /// Some implementations also include `routes`. We don't use it but accept it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<serde_json::Value>,
}

/// Some LNURL endpoints return `{"status":"ERROR","reason":"..."}` instead
/// of the documented invoice shape. We try this when JSON-decoding the
/// invoice payload fails.
#[derive(Debug, Clone, Deserialize)]
pub struct LnurlErrorPayload {
    pub status: String,
    #[serde(default)]
    pub reason: String,
}

/// A parsed Lightning Address (LUD-16): `local_part@domain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightningAddress {
    pub local_part: String,
    pub domain: String,
}

impl LightningAddress {
    /// URL for the LNURL-pay metadata document.
    ///
    /// LUD-16 says HTTPS, but RFC 6761 special-use names — `localhost`,
    /// `*.localhost`, `*.local`, `*.test` — are routinely served over plain
    /// HTTP for local development and integration testing (mm-fakestripe in
    /// our case). Browsers treat them the same way, so we mirror that
    /// behaviour to keep the demo path runnable without TLS infrastructure.
    pub fn well_known_url(&self) -> String {
        let scheme = if self.is_local_domain() { "http" } else { "https" };
        format!(
            "{}://{}/.well-known/lnurlp/{}",
            scheme, self.domain, self.local_part
        )
    }

    fn is_local_domain(&self) -> bool {
        let host = self.domain.split(':').next().unwrap_or(self.domain.as_str());
        host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host.ends_with(".test")
    }
}

/// Parse a Lightning Address (LUD-16: `local_part@domain.tld`).
///
/// Validation deliberately stays light:
/// - exactly one `@`
/// - both halves non-empty
/// - local part: `[a-z0-9._-]+` (lowercased to match LUD-16 §"Identifier validation")
/// - domain: at least one dot, no spaces, no `@`
///
/// We don't attempt full RFC 1035 — operators that publish weird
/// addresses fail at resolution time, which is fine for the demo.
pub fn parse_lightning_address(input: &str) -> Result<LightningAddress, LnurlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(LnurlError::InvalidAddress("empty".into()));
    }

    let (local, domain) = match trimmed.split_once('@') {
        Some(parts) => parts,
        None => return Err(LnurlError::InvalidAddress("missing '@'".into())),
    };

    if local.is_empty() || domain.is_empty() {
        return Err(LnurlError::InvalidAddress(
            "local-part or domain is empty".into(),
        ));
    }

    if local.contains('@') || domain.contains('@') {
        return Err(LnurlError::InvalidAddress("multiple '@'".into()));
    }

    // Allow `host:port` for local dev (fakeln on `localhost:8787`).
    let domain_host = domain.split(':').next().unwrap_or(domain);
    let is_local_host = domain_host == "localhost"
        || domain_host == "127.0.0.1"
        || domain_host == "::1"
        || domain_host.ends_with(".localhost")
        || domain_host.ends_with(".local")
        || domain_host.ends_with(".test");
    if !is_local_host && !domain.contains('.') {
        return Err(LnurlError::InvalidAddress("domain has no '.'".into()));
    }

    if domain.chars().any(|c| c.is_whitespace())
        || local.chars().any(|c| c.is_whitespace())
    {
        return Err(LnurlError::InvalidAddress("whitespace not allowed".into()));
    }

    // LUD-16 lowercase normalisation.
    let local_norm = local.to_ascii_lowercase();
    let allowed_local = local_norm
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if !allowed_local {
        return Err(LnurlError::InvalidAddress(
            "local-part contains disallowed character".into(),
        ));
    }

    Ok(LightningAddress {
        local_part: local_norm,
        domain: domain.to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_address() {
        let a = parse_lightning_address("alice@phoenix.acinq.co").unwrap();
        assert_eq!(a.local_part, "alice");
        assert_eq!(a.domain, "phoenix.acinq.co");
        assert_eq!(
            a.well_known_url(),
            "https://phoenix.acinq.co/.well-known/lnurlp/alice"
        );
    }

    #[test]
    fn parse_lowercases_address() {
        let a = parse_lightning_address("Alice+Tips@Phoenix.ACINQ.co").unwrap();
        assert_eq!(a.local_part, "alice+tips");
        assert_eq!(a.domain, "phoenix.acinq.co");
    }

    #[test]
    fn parse_trims_whitespace() {
        let a = parse_lightning_address("  bob@example.org  ").unwrap();
        assert_eq!(a.local_part, "bob");
        assert_eq!(a.domain, "example.org");
    }

    #[test]
    fn rejects_missing_at_sign() {
        assert!(matches!(
            parse_lightning_address("aliceexample.com"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            parse_lightning_address(""),
            Err(LnurlError::InvalidAddress(_))
        ));
        assert!(matches!(
            parse_lightning_address("@example.com"),
            Err(LnurlError::InvalidAddress(_))
        ));
        assert!(matches!(
            parse_lightning_address("alice@"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn rejects_multiple_at_signs() {
        assert!(matches!(
            parse_lightning_address("a@b@c.com"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn rejects_domain_without_dot() {
        // Plain "localhost" without a dot fails ONLY when it's not a reserved
        // RFC 6761 name; bare "localhost" IS reserved, so we accept it.
        assert!(matches!(
            parse_lightning_address("alice@notadomain"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn accepts_localhost_and_loopback_for_dev() {
        assert!(parse_lightning_address("alice@localhost").is_ok());
        assert!(parse_lightning_address("alice@localhost:8787").is_ok());
        assert!(parse_lightning_address("alice@127.0.0.1:8787").is_ok());
        assert!(parse_lightning_address("alice@fakeln.test").is_ok());
        assert!(parse_lightning_address("alice@dev.local").is_ok());
    }

    #[test]
    fn well_known_url_uses_http_for_local_domains() {
        let a = parse_lightning_address("alice@localhost:8787").unwrap();
        assert_eq!(
            a.well_known_url(),
            "http://localhost:8787/.well-known/lnurlp/alice"
        );
        let b = parse_lightning_address("bob@fakeln.test").unwrap();
        assert_eq!(
            b.well_known_url(),
            "http://fakeln.test/.well-known/lnurlp/bob"
        );
    }

    #[test]
    fn rejects_internal_whitespace() {
        assert!(matches!(
            parse_lightning_address("alice @example.com"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn rejects_disallowed_characters() {
        assert!(matches!(
            parse_lightning_address("alice/bob@example.com"),
            Err(LnurlError::InvalidAddress(_))
        ));
    }

    #[test]
    fn metadata_validate_accepts_normal() {
        let m = LnurlPayMetadata {
            callback: "https://example.com/cb".into(),
            max_sendable: 100_000_000,
            min_sendable: 1_000,
            metadata: "[]".into(),
            tag: "payRequest".into(),
            comment_allowed: 0,
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn metadata_validate_rejects_wrong_tag() {
        let m = LnurlPayMetadata {
            callback: "https://example.com/cb".into(),
            max_sendable: 100,
            min_sendable: 10,
            metadata: "[]".into(),
            tag: "withdrawRequest".into(),
            comment_allowed: 0,
        };
        assert!(matches!(
            m.validate(),
            Err(LnurlError::InvalidMetadata(_))
        ));
    }

    #[test]
    fn metadata_validate_rejects_inverted_bounds() {
        let m = LnurlPayMetadata {
            callback: "https://example.com/cb".into(),
            max_sendable: 100,
            min_sendable: 200,
            metadata: "[]".into(),
            tag: "payRequest".into(),
            comment_allowed: 0,
        };
        assert!(matches!(
            m.validate(),
            Err(LnurlError::InvalidMetadata(_))
        ));
    }

    #[test]
    fn metadata_validate_rejects_zero_min() {
        let m = LnurlPayMetadata {
            callback: "https://example.com/cb".into(),
            max_sendable: 100,
            min_sendable: 0,
            metadata: "[]".into(),
            tag: "payRequest".into(),
            comment_allowed: 0,
        };
        assert!(matches!(
            m.validate(),
            Err(LnurlError::InvalidMetadata(_))
        ));
    }

    #[test]
    fn metadata_deserialises_real_world_shape() {
        // Shape Phoenix / Wallet of Satoshi actually return.
        let raw = r#"{
            "callback": "https://api.example.com/lnurlp/alice/cb",
            "maxSendable": 100000000000,
            "minSendable": 1000,
            "metadata": "[[\"text/plain\",\"Sats for alice\"],[\"text/identifier\",\"alice@example.com\"]]",
            "tag": "payRequest",
            "commentAllowed": 144
        }"#;
        let m: LnurlPayMetadata = serde_json::from_str(raw).unwrap();
        m.validate().unwrap();
        assert_eq!(m.comment_allowed, 144);
        assert!(m.metadata.contains("alice@example.com"));
    }

    #[test]
    fn invoice_deserialises_minimal_shape() {
        let raw = r#"{"pr":"lnbc100n1pjabcd...","routes":[]}"#;
        let inv: LnurlPayInvoice = serde_json::from_str(raw).unwrap();
        assert!(inv.pr.starts_with("lnbc"));
        assert!(inv.success_action.is_none());
    }

    #[test]
    fn invoice_deserialises_with_success_action() {
        let raw = r#"{"pr":"lnbc1...","successAction":{"tag":"message","message":"Thanks!"}}"#;
        let inv: LnurlPayInvoice = serde_json::from_str(raw).unwrap();
        assert!(inv.success_action.is_some());
    }

    #[test]
    fn error_payload_decodes() {
        let raw = r#"{"status":"ERROR","reason":"out of liquidity"}"#;
        let err: LnurlErrorPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(err.status, "ERROR");
        assert_eq!(err.reason, "out of liquidity");
    }
}
