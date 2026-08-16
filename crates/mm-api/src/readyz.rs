//! Unauthenticated readiness probe for orchestrators.
//!
//! `GET /readyz` on the client listener: `200 "ok"` when the database
//! answers, `503 "unready"` when it doesn't. Deliberately outside
//! `/_mm/client/v1` and the OpenAPI surface — this is infrastructure, not
//! client API — and deliberately *bare*: an unauthenticated endpoint must not
//! leak component internals (the rich per-component health stays on the
//! authenticated admin `/health`).
//!
//! Liveness probes must NOT point here: readiness losing the DB should pull
//! the pod from the Service, not restart it (a restart storm on top of a DB
//! outage helps nobody). Point liveness at a static route such as
//! `/.well-known/matrix/matrixmedia`.

use axum::{Router, extract::State, http::StatusCode, routing::get};

use crate::state::SharedState;

/// Map the DB health outcome to the probe response. Pure — unit-tested
/// directly; the handler is a trivial shim over it.
pub(crate) fn readyz_response(db_ok: bool) -> (StatusCode, &'static str) {
    if db_ok {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unready")
    }
}

async fn readyz(State(state): State<SharedState>) -> (StatusCode, &'static str) {
    let ok = state.db.health_check().await.is_ok();
    if !ok {
        // The 503 itself is silent by design; the reason goes to logs.
        tracing::warn!("readyz: database health check failed");
    }
    readyz_response(ok)
}

/// Router for the readiness probe. Mounted by mm-server on the client
/// listener, alongside the well-known routes.
pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_db_maps_to_200_ok() {
        let (code, body) = readyz_response(true);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[test]
    fn failed_db_maps_to_503_without_detail() {
        let (code, body) = readyz_response(false);
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        // Bare body — must not carry error internals (unauthenticated surface).
        assert_eq!(body, "unready");
    }
}
