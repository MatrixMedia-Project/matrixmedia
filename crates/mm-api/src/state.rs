use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use mm_core::cache::{RedisCache, TokenCache};
use mm_core::config::Config;
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_payment::EntitlementService;
use mm_payment::PaymentProviderRegistry;
use mm_sfu::SfuAdapter;
use sqlx::PgPool;

/// Shared application state for all handlers.
///
/// Contains the four backend pillars (DB, SFU, homeserver client, token cache)
/// plus the server configuration, appservice handler, and Prometheus metrics.
pub struct AppState {
    /// Unified database abstraction (PostgreSQL via PgDatabase).
    pub db: Box<dyn Database>,
    /// SFU adapter (LiveKit with circuit breaker).
    pub sfu: Box<dyn SfuAdapter>,
    /// Matrix homeserver client for bot actions and OpenID validation.
    pub hs_client: HomeserverClient,
    /// Token validation cache (SHA-256(token) -> user_id).
    pub token_cache: TokenCache,
    /// Server configuration.
    pub config: Config,
    /// Appservice handler for incoming homeserver transactions.
    pub appservice_handler: AppserviceHandler,
    /// Prometheus metrics.
    pub metrics: Metrics,
    /// Time the server started (for uptime reporting).
    pub started_at: std::time::Instant,
    /// PostgreSQL connection pool for direct PG queries by services
    /// that need raw pool access (e.g. EntitlementService, TrendingEngine).
    /// Populated from PgDatabase when monetization is enabled.
    pub pg_pool: Option<PgPool>,
    /// Stripe API client.
    /// `None` when `monetization.enabled = false`.
    pub stripe_client: Option<stripe::Client>,
    /// Payment provider registry (Stripe, future: PayPal, etc.).
    /// `None` when `monetization.enabled = false`.
    pub payment_registry: Option<Arc<PaymentProviderRegistry>>,
    /// Entitlement service for subscription-based content gating.
    /// `None` when `monetization.enabled = false` or subscriptions disabled.
    pub entitlement_service: Option<Arc<EntitlementService>>,
    /// Redis cache layer for cross-instance shared caching.
    /// `None` when `MM_REDIS_URL` is not configured (falls back to moka).
    pub redis: Option<Arc<RedisCache>>,
    /// Advertising decision engine.
    /// `None` when `advertising.enabled = false`.
    pub ad_engine: Option<Arc<mm_ads::AdDecisionEngine>>,
    /// Media switch client for ad injection via WebRTC source switching.
    /// `None` when `MM_SWITCH_URL` is not configured.
    pub switch_client: Option<Arc<mm_core::switch_client::SwitchClient>>,
    /// HMAC secret for signing mm-switch auth tokens.
    /// `None` when `MM_SWITCH_AUTH_SECRET` is not configured.
    pub switch_auth_secret: Option<String>,
    /// In-flight ad switches (impression_token → switch state).
    /// Used by `report_ad_event` skip/complete to immediately route the viewer
    /// back to the live stream and remove the per-viewer ad source.
    pub ad_switches: Arc<Mutex<HashMap<String, AdSwitchEntry>>>,
}

/// Per-impression record of an active mm-switch ad routing.
#[derive(Debug, Clone)]
pub struct AdSwitchEntry {
    pub viewer_id: String,
    pub stream_source_id: String,
    pub ad_source_id: String,
    /// When the ad started — used to enforce minimum view time.
    pub started_at: std::time::Instant,
}

/// Type alias for the shared state passed to handlers via `axum::extract::State`.
pub type SharedState = Arc<AppState>;
