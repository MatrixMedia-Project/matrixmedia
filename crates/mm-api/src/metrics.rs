use axum::extract::State;
use axum::response::IntoResponse;
use prometheus::Encoder;

use crate::state::SharedState;

/// GET /metrics -- Prometheus text-format metrics endpoint.
///
/// Encodes all registered metric families from the application's Prometheus
/// registry and returns them with the standard Prometheus content type.
pub async fn metrics_handler(State(state): State<SharedState>) -> impl IntoResponse {
    let encoder = prometheus::TextEncoder::new();
    let mut buffer = Vec::new();
    encoder
        .encode(&state.metrics.registry.gather(), &mut buffer)
        .unwrap_or_default();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        String::from_utf8(buffer).unwrap_or_default(),
    )
}
