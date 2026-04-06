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
}

/// E2EE capability descriptor for the `.well-known` response.
#[derive(Debug, Clone, Serialize)]
pub struct E2eeSupport {
    pub enabled: bool,
    pub required: bool,
    pub algorithms: Vec<String>,
}

/// Build the router that exposes the `.well-known` endpoint.
///
/// The endpoint is mounted at `/.well-known/matrix/matrixmedia` and must be
/// reachable on the public-facing port (the client port) so that federated
/// clients can resolve it.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/.well-known/matrix/matrixmedia", get(wellknown_handler))
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
        },
    }
}
