pub mod admin;
pub mod ads;
pub mod analytics;
pub mod announcements;
pub mod appservice;
pub mod auth_signup;
pub mod client;
pub mod client_ip;
pub mod creator;
pub mod discovery;
pub mod error;
pub mod feed;
pub mod honeypot;
pub mod reserved_names;
pub mod rooms;
mod guards;
pub mod metrics;
pub mod middleware;
pub mod monetization;
pub mod rate_limit;
pub mod state;
pub mod wellknown;
pub mod widget;

use axum::Router;
use tower_http::services::ServeDir;

use state::SharedState;

/// Build the client/widget API router (`/_mm/client/v1/` + `/_mm/widget/v1/`).
///
/// All routes receive the shared application state via `axum::extract::State`.
///
/// Monetization routes are always mounted; the handlers check
/// `config.monetization.enabled` and return 501 when the feature is off,
/// which is clearer than a generic 404 for a known API surface.
pub fn client_router(state: SharedState) -> Router {
    Router::new()
        .nest("/_mm/client/v1", client::routes(state.clone()))
        .nest("/_mm/widget/v1", widget::routes(state.clone()))
        .nest("/_mm/appservice", appservice::routes(state.clone()))
        // Monetization: authenticated endpoints under /_mm/client/v1/,
        // unauthenticated webhook under /_mm/webhooks/.
        // Handlers guard on monetization.enabled (returns 501 when off).
        .nest("/_mm/client/v1", monetization::routes(state.clone()))
        .nest("/_mm/webhooks", monetization::webhook_routes(state.clone()))
        // Phase 7c: Discovery & Recommendations
        .nest("/_mm/client/v1", discovery::routes(state.clone()))
        // Phase 9: Advertising
        .nest("/_mm/client/v1", ads::routes(state.clone()))
        // Phase 12: Creator self-service
        .nest("/_mm/client/v1", creator::routes(state.clone()))
        // Phase 12: Creator analytics (per-room earnings/streams/donors)
        .nest("/_mm/client/v1", analytics::routes(state.clone()))
        // Phase 12: Room-level controls (stream perms + enable-mm)
        .nest("/_mm/client/v1", rooms::routes(state.clone()))
        // Phase 13: Account signup (unauthenticated registration endpoints)
        .nest("/mm/v1", auth_signup::routes(state.clone()))
        // Phase 14: Server announcements (unauthenticated public GET)
        .nest("/mm/v1", announcements::routes(state.clone()))
        // Phase 15: Newsfeed (authenticated; dark-launch gated)
        .nest("/_mm/client/v1", feed::routes(state.clone()))
        // Admin routes also accessible on client port (for dev test client)
        .nest("/_mm/admin/v1", admin::routes(state))
}

/// Build the admin API router (`/_mm/admin/v1/`).
///
/// All routes receive the shared application state via `axum::extract::State`.
pub fn admin_router(state: SharedState) -> Router {
    Router::new().nest("/_mm/admin/v1", admin::routes(state))
}

/// Build a router that serves static widget files from `widget_dir`.
///
/// The files are served at `/_mm/widget/` with `index.html` appended
/// automatically for directory requests.
pub fn widget_static_router(widget_dir: &str) -> Router {
    Router::new().nest_service(
        "/_mm/widget",
        ServeDir::new(widget_dir).append_index_html_on_directories(true),
    )
}

/// Build the Prometheus metrics router.
///
/// Exposes a single `GET /metrics` endpoint that returns the text-format
/// metric families from the application's Prometheus registry.
pub fn metrics_router(state: SharedState) -> Router {
    Router::new()
        .route("/metrics", axum::routing::get(metrics::metrics_handler))
        .with_state(state)
}
