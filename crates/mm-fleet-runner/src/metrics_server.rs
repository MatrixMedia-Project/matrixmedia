//! `/metrics` on `MM_FLEET_RUNNER_LISTEN` (spec §8.5), bound to the docker network only.
//!
//! It is the one thing the runner serves, and it serves nothing else: no other path is routed,
//! and the registry holds the runner's own collectors and a few of mm-core's, never a secret, a
//! token or a provider's error text (a label is a provider id, a zone or a fixed outcome).

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prometheus::{Encoder, Registry, TextEncoder};
use tokio_util::sync::CancellationToken;

/// The collectors the runner publishes: its own (`mm_fleet::metrics`) and the three of mm-core's
/// that the runner's loops set (its sweepers' two counters and the loop heartbeats). Not
/// `mm_fleet_nodes`, which mm-core's planner publishes, and not mm-core's fleet health gauges
/// (the heartbeat age and the like), which only mm-core computes from the database and
/// publishes on its own endpoint.
pub fn registry() -> Registry {
    use mm_core::metrics_global as g;
    let r = Registry::new();
    mm_fleet::metrics::register_runner(&r).expect("the runner's collectors register once");
    r.register(Box::new(g::FLEET_ORPHANS_DESTROYED.clone()))
        .expect("mm_fleet_orphans_destroyed_total registers once");
    r.register(Box::new(g::FLEET_REAPER_DEADLINE_KILLS.clone()))
        .expect("mm_fleet_reaper_deadline_kills_total registers once");
    r.register(Box::new(g::BACKGROUND_TASK_HEARTBEAT.clone()))
        .expect("the loop heartbeats register once");
    r
}

/// Binds `addr` and answers `GET /metrics` until `cancel` fires. Returns the bind error, so the
/// caller can say which address could not be served.
pub async fn serve(
    addr: SocketAddr,
    registry: Registry,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(registry);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
}

async fn scrape(State(registry): State<Registry>) -> Response {
    let mut body = Vec::new();
    match TextEncoder::new().encode(&registry.gather(), &mut body) {
        Ok(()) => ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "cannot encode the metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
