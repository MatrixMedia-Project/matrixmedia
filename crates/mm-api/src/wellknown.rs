//! `.well-known` service discovery endpoint.
//!
//! This allows federated clients to discover the MM server's URL and
//! capabilities without needing a hardcoded configuration. A client that
//! reads a `com.matrixmedia.stream` state event with a `mm_server_url` field
//! can hit `{mm_server_url}/.well-known/matrix/matrixmedia` to confirm the
//! server's identity, protocol version, and supported features before
//! attempting to authenticate and join.

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use crate::state::SharedState;

/// Top-level response body for `/.well-known/matrix/matrixmedia`.
#[derive(Debug, Clone, Serialize)]
pub struct WellKnownResponse {
    pub mm_server: MMServerInfo,
}

/// MM server descriptor returned by the `.well-known` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct MMServerInfo {
    /// The public URL of this MM server (used by federated clients).
    pub base_url: String,
    /// MatrixMedia protocol/server version (Cargo package version).
    pub version: String,
    /// Server name of the Matrix homeserver this MM instance is bound to.
    pub matrix_server: String,
    /// Whether federation is enabled on this MM server.
    pub federation_enabled: bool,
    /// E2EE support on this server.
    pub e2ee: E2eeSupport,
    /// Whether recording is enabled.
    pub recording_enabled: bool,
    /// Operator manifest payment metadata (M1 pilot addition).
    /// Other operators + clients read this to discover monetization rails
    /// before initiating tip / subscription flows.
    pub payment: PaymentManifest,
}

/// E2EE capability descriptor for the `.well-known` response.
#[derive(Debug, Clone, Serialize)]
pub struct E2eeSupport {
    pub enabled: bool,
    pub required: bool,
    pub algorithms: Vec<String>,
}

/// Operator-declared payment posture, embedded in the manifest.
///
/// Per `mm-final-design.md` §6: this is the load-bearing operator declaration
/// that other servers + clients use to decide whether to initiate monetization
/// flows. `custody.model` is the most important field — see `mica-mm-implications.md`
/// for why non-custodial is the default in EU.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentManifest {
    /// Manifest schema version. Bumped on breaking changes.
    pub schema_version: u32,
    /// Custody posture (how operator handles user funds).
    pub custody: CustodyDescriptor,
    /// Active payment-provider descriptors. Empty array = monetization not enabled.
    pub providers: Vec<PaymentProviderDescriptor>,
    /// Versions of the cross-server tip protocol this server speaks.
    /// Empty = no Lightning tipping support.
    pub tip_protocol_versions: Vec<u32>,
}

/// Custody posture declaration — the operator's stance on user-fund handling.
#[derive(Debug, Clone, Serialize)]
pub struct CustodyDescriptor {
    /// "non-custodial" | "custodial-licensed" | "unknown"
    pub model: String,
    /// CASP / VASP / MSB license number, when custody.model == "custodial-licensed".
    /// Operators MUST declare this for federation peers to trust custodial flows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_number: Option<String>,
    /// Jurisdiction issuing the license (ISO 3166-1 alpha-2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jurisdiction: Option<String>,
}

/// Single payment-provider descriptor.
#[derive(Debug, Clone, Serialize)]
pub struct PaymentProviderDescriptor {
    /// Provider id matching the registry key (e.g. "stripe", "lightning", "mock").
    pub id: String,
    /// Provider type taxonomy: "fiat_processor" | "lightning" | "fiat_to_crypto_onramp"
    /// | "mobile_money" | "stablecoin".
    #[serde(rename = "type")]
    pub provider_type: String,
    /// Operations this provider supports for this operator.
    /// Subset of: "tip" | "subscription" | "wallet_topup".
    pub supports: Vec<String>,
}

/// Build the router that exposes the `.well-known` endpoints.
///
/// Two routes:
/// - `/.well-known/matrix/matrixmedia` (legacy / Matrix-style namespace)
/// - `/.well-known/matrixmedia/operator.json` (canonical per `mm-final-design.md` §6)
///
/// Both return identical content. Dual route ships during M1 for compatibility;
/// the matrix-style namespace can be deprecated post-M3.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/.well-known/matrix/matrixmedia", get(wellknown_handler))
        .route(
            "/.well-known/matrixmedia/operator.json",
            get(wellknown_handler),
        )
        .with_state(state)
}

async fn wellknown_handler(State(state): State<SharedState>) -> Json<WellKnownResponse> {
    Json(build_response(&state))
}

/// Build the `.well-known` response body from the server configuration.
///
/// Extracted into a pure function so it can be exercised in unit tests
/// without needing to stand up a live Axum server.
pub fn build_response(state: &SharedState) -> WellKnownResponse {
    WellKnownResponse {
        mm_server: MMServerInfo {
            base_url: state
                .config
                .server
                .public_url
                .clone()
                .unwrap_or_else(|| "http://localhost:6167".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            matrix_server: state.config.matrix.server_name.clone(),
            federation_enabled: state.config.federation.enabled,
            e2ee: E2eeSupport {
                enabled: state.config.e2ee.enabled,
                required: state.config.e2ee.required,
                algorithms: vec![state.config.e2ee.algorithm.clone()],
            },
            recording_enabled: state.config.recording.enabled,
            payment: build_payment_manifest(state),
        },
    }
}

/// Build the payment portion of the operator manifest from runtime state.
///
/// Reads the active payment-provider registry to advertise which rails are
/// available. Always includes the synthetic `lnurl-pay` rail when monetization
/// is enabled — that path lives in `mm-payment::lnurl` and bypasses the
/// registry because the operator runs no Lightning node (per ADR-0007 + the
/// true-P2P pivot). Defaults to `non-custodial` posture — operators that
/// want to declare a CASP license must override via config (M4 work).
fn build_payment_manifest(state: &SharedState) -> PaymentManifest {
    let mut providers = state
        .payment_registry
        .as_ref()
        .map(|r| {
            r.available_providers()
                .into_iter()
                .map(|id| {
                    let (provider_type, supports) = classify_provider(id);
                    PaymentProviderDescriptor {
                        id: id.to_owned(),
                        provider_type,
                        supports,
                    }
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // LNURL-pay (LUD-06 + LUD-16) is the canonical Lightning rail. It works
    // for any creator that publishes a Lightning Address, regardless of which
    // (if any) Lightning provider lives in the registry. Surface it in the
    // manifest so federated peers know tipping is available even on operators
    // that run no Lightning node themselves.
    if state.payment_registry.is_some() {
        providers.push(PaymentProviderDescriptor {
            id: "lnurl-pay".to_owned(),
            provider_type: "lightning_p2p".to_owned(),
            supports: vec!["tip".to_owned()],
        });
    }

    // Tip protocol version 1 supported when *any* Lightning-capable rail
    // (LNURL-pay, LNBits, or a future provider) is available. In practice
    // this is true whenever the registry is initialised, since LNURL-pay is
    // always synthesised above.
    let tip_protocol_versions = if providers.iter().any(|p| {
        p.provider_type == "lightning"
            || p.provider_type == "lightning_p2p"
            || p.supports.iter().any(|s| s == "tip")
    }) {
        vec![1]
    } else {
        vec![]
    };

    PaymentManifest {
        schema_version: 1,
        custody: CustodyDescriptor {
            // Default non-custodial. Custodial mode is gated on operator providing
            // a CASP license (M4 startup check); when set, this becomes
            // "custodial-licensed" with the license_number/jurisdiction populated.
            model: "non-custodial".to_owned(),
            license_number: None,
            jurisdiction: None,
        },
        providers,
        tip_protocol_versions,
    }
}

/// Map a registered provider id to its taxonomy + supported operations.
/// Conservative: unknown providers get the most restrictive defaults.
fn classify_provider(id: &str) -> (String, Vec<String>) {
    match id {
        "stripe" => (
            "fiat_processor".to_owned(),
            vec!["tip".to_owned(), "subscription".to_owned()],
        ),
        "lightning" | "lnbits" => ("lightning".to_owned(), vec!["tip".to_owned()]),
        "lnurl-pay" => ("lightning_p2p".to_owned(), vec!["tip".to_owned()]),
        "mock" => (
            "fiat_processor".to_owned(),
            vec!["tip".to_owned(), "subscription".to_owned()],
        ),
        _ => ("unknown".to_owned(), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_providers() {
        let (t, s) = classify_provider("stripe");
        assert_eq!(t, "fiat_processor");
        assert!(s.contains(&"tip".to_owned()));
        assert!(s.contains(&"subscription".to_owned()));

        let (t, s) = classify_provider("lightning");
        assert_eq!(t, "lightning");
        assert_eq!(s, vec!["tip"]);

        let (t, s) = classify_provider("lnbits");
        assert_eq!(t, "lightning");
        assert_eq!(s, vec!["tip"]);
    }

    #[test]
    fn classify_lnurl_pay_is_lightning_p2p() {
        let (t, s) = classify_provider("lnurl-pay");
        assert_eq!(t, "lightning_p2p");
        assert_eq!(s, vec!["tip"]);
    }

    #[test]
    fn classify_unknown_provider_is_safe_default() {
        let (t, s) = classify_provider("some_future_provider");
        assert_eq!(t, "unknown");
        assert!(s.is_empty(), "Unknown providers must claim no capabilities");
    }

    #[test]
    fn payment_manifest_serializes_with_skip_none() {
        let custody = CustodyDescriptor {
            model: "non-custodial".to_owned(),
            license_number: None,
            jurisdiction: None,
        };
        let manifest = PaymentManifest {
            schema_version: 1,
            custody,
            providers: vec![],
            tip_protocol_versions: vec![],
        };
        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["custody"]["model"], "non-custodial");
        // license_number + jurisdiction must be omitted when None
        assert!(json["custody"].get("license_number").is_none());
        assert!(json["custody"].get("jurisdiction").is_none());
    }
}
