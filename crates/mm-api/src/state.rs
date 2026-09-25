use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use mm_core::cache::{RedisCache, TokenCache};
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_matrix::appservice::AppserviceHandler;
use mm_matrix::client::HomeserverClient;
use mm_payment::EntitlementService;
use mm_payment::PaymentProviderRegistry;
use mm_payment::lnurl::LnurlPayClient;
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
    /// Live configuration. Read it with [`AppState::config`] — once per request.
    pub config_handle: mm_core::config_handle::ConfigHandle,
    /// Dashboard-managed settings (import, overlay, writes, poll, restart).
    pub settings: std::sync::Arc<crate::settings_service::SettingsService>,
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
    /// LNURL-pay client used to resolve creator Lightning Addresses into
    /// fresh BOLT11 invoices on every donation. Always constructed (cheap +
    /// stateless); only used when `monetization.enabled` and the creator has
    /// `lightning_address` set.
    pub lnurl_client: LnurlPayClient,
    /// Entitlement service for subscription-based content gating.
    /// `None` when `monetization.enabled = false` or subscriptions disabled.
    pub entitlement_service: Option<Arc<EntitlementService>>,
    /// Redis cache layer for cross-instance shared caching.
    /// `None` when `MM_REDIS_URL` is not configured (falls back to moka).
    pub redis: Option<Arc<RedisCache>>,
    /// Advertising decision engine.
    /// `None` when `advertising.enabled = false`.
    pub ad_engine: Option<Arc<mm_ads::AdDecisionEngine>>,
    /// Resolves which mm-switch instance a call belongs to (WS-A Task 4).
    ///
    /// `None` means the switch feature is not configured at all (`MM_SWITCH_URL`
    /// unset) — a different thing from `Some(pool)` holding zero fleet nodes,
    /// which is an ordinary single-host install serving everyone from its
    /// origin. Collapsing the two would make a disabled switch
    /// indistinguishable from a fleetless one, and the handlers that hand
    /// clients a `switch_url` depend on that distinction.
    pub switch_pool: Option<Arc<crate::switch_pool::SwitchPool>>,
    /// Last Broadcast servers snapshot, written by the 10 s collector, read by the admin route.
    pub broadcast_servers: Arc<crate::broadcast_servers::SnapshotCell>,
    /// In-flight ad switches (impression_token → switch state).
    /// Used by `report_ad_event` skip/complete to immediately route the viewer
    /// back to the live stream and remove the per-viewer ad source.
    pub ad_switches: Arc<Mutex<HashMap<String, AdSwitchEntry>>>,
    /// Per-IP rate limiter for signup (`POST /_mm/client/v1/register`).
    /// Quota: `config.matrix.signup_rate_limit_per_ip_per_hour` per hour (default 5),
    /// read live on every request (Live setting).
    pub signup_limiter: crate::rate_limit::LiveQuotaLimiter,
    /// Per-IP rate limiter for username-availability checks
    /// (`GET /_mm/client/v1/register/available`).
    /// Quota: 60/hr — generous, just abuse defense.
    pub signup_avail_limiter: crate::rate_limit::SignupRateLimiter,
    /// Synapse admin shared-secret client for user provisioning.
    pub synapse_admin: std::sync::Arc<mm_core::synapse_admin::SynapseAdminClient>,
    /// PostgreSQL pool always available for signup audit writes (independent of
    /// `monetization.enabled`; mirrors `pg_pool` but is never `None`).
    pub signup_pool: sqlx::PgPool,
    /// Moka cache for the single active announcement.
    ///
    /// 30s TTL + max capacity 1 — this is a single hot row that every active
    /// client polls every 60s. The cache eliminates per-poll DB hits in steady
    /// state and powers ETag round-trips. Admin POST/DELETE handlers MUST
    /// call `announcement_cache.invalidate(&()).await` so the next client
    /// poll cycle sees the change without waiting up to 30s.
    pub announcement_cache:
        Arc<moka::future::Cache<(), Option<mm_db::announcements::AnnouncementRow>>>,
    /// Per-user 30s cache for `com.steegler.matrixmedia.feed_muted` room
    /// lists, fetched via the Matrix account_data API. Keeps `GET /feed`
    /// reads cheap (one DB query) instead of round-tripping Synapse on
    /// every page request.
    pub feed_cache: Arc<moka::future::Cache<String, Vec<String>>>,
    /// Per-user rate limiter for `/feed` reads (30 req/min ≈ 1800/hr).
    pub feed_limiter: crate::rate_limit::SignupRateLimiter,
    /// Per-reporter rate limit for POST /_mm/client/v1/moderation/report.
    pub moderation_report_limiter: crate::rate_limit::SignupRateLimiter,
    /// 60s cache for the (subscriber, room) → effective `TierPermissions`
    /// resolution used by the `require_permission` gate. Permission/tier
    /// changes propagate within a minute (no realtime push, by design).
    /// Key: `(subscriber_user_id, room_id)`.
    pub permissions_cache:
        Arc<moka::future::Cache<(String, String), mm_core::permissions::TierPermissions>>,
    /// Trending engine, built once at startup.
    ///
    /// It owns a 5-minute moka cache AND rewrites the `mm_trending_cache` table on every
    /// recalculation. The discovery handlers used to construct one per request, so the
    /// cache could never hit: every call to the (unauthenticated) trending endpoint ran a
    /// full recalculation plus the cache-table rewrite. Shared here, a recalculation
    /// happens at most once per 5 minutes no matter the request rate.
    ///
    /// `None` when `monetization.enabled = false` — the engine needs the PG pool, and the
    /// handlers that use it already return early in that case.
    pub trending_engine: Option<Arc<mm_recommendations::trending::TrendingEngine>>,
}

impl AppState {
    /// One consistent snapshot of the running configuration. Take it ONCE per
    /// request (or per background-loop tick) and read everything from it.
    pub fn config(&self) -> std::sync::Arc<mm_core::config::Config> {
        self.config_handle.load()
    }

    /// The origin switch: ingest, recording and source registration.
    ///
    /// Recording in particular cannot move. The publisher publishes to the
    /// origin and the recorder taps that fan-out, so a recording started
    /// elsewhere would have no source to tap.
    pub fn origin_switch(&self) -> Option<Arc<mm_core::switch_client::SwitchClient>> {
        self.switch_pool.as_ref().map(|p| p.origin())
    }

    /// [`Self::origin_switch`] borrowed from the state, for contexts that hold a
    /// reference for a whole call (`stream_lifecycle::EndContext`).
    pub fn origin_switch_ref(&self) -> Option<&Arc<mm_core::switch_client::SwitchClient>> {
        self.switch_pool.as_deref().map(crate::switch_pool::SwitchPool::origin_ref)
    }

    /// Where a viewer-scoped call (an ad switch) should go, and whether its
    /// outcome may be billed. `None` only when the switch feature is off.
    pub async fn route_viewer(
        &self,
        viewer: &mm_core::fleet::ViewerId,
    ) -> Option<crate::switch_pool::ViewerRoute> {
        let pool = self.switch_pool.as_ref()?;
        Some(pool.route_viewer(viewer).await)
    }

    /// The client for the node an ad routing was set up on, so the switch-back
    /// and the source cleanup reach the same place. `None` when that node is gone.
    pub async fn switch_for_node(
        &self,
        node_id: Option<&mm_core::fleet::NodeId>,
    ) -> Option<Arc<mm_core::switch_client::SwitchClient>> {
        self.switch_pool.as_ref()?.client_for_node(node_id).await
    }
}

/// Per-impression record of an active mm-switch ad routing.
#[derive(Debug, Clone)]
pub struct AdSwitchEntry {
    pub viewer_id: String,
    pub stream_source_id: String,
    pub ad_source_id: String,
    /// When the ad started — used to enforce minimum view time.
    pub started_at: std::time::Instant,
    /// The node this routing was set up on (WS-A Task 5, FR-405).
    ///
    /// The switch-back and the source cleanup MUST go to the same node as the
    /// original switch. Sending either elsewhere does not error — mm-switch does
    /// not know the viewer or the source and no-ops — so the viewer would sit on
    /// the ad forever and the ad's FileSource would leak on the node that has it.
    /// `None` means the routing was set up on the origin.
    pub node_id: Option<mm_core::fleet::NodeId>,
    /// Which break this ad filled. Recorded so an impression can be attributed
    /// to a slot without re-deriving it from timing.
    pub slot: mm_ads::creative::AdSlot,
    /// Whether this impression may be billed.
    ///
    /// False when the switch could not be routed to the viewer's own node. The
    /// ad was never shown in that case, and an impression recorded anyway is a
    /// charge to an advertiser for nothing.
    pub billable: bool,
}

impl AdSwitchEntry {
    /// A pre-roll shown because a viewer was migrated between nodes, not because
    /// an ad break was due (FR-410, design §16.1).
    ///
    /// `billable = false`: the viewer is already inside a paid-for viewing
    /// session, and a migration is our operational business, not theirs. Charging
    /// an advertiser for it would bill the same viewer twice for one programme,
    /// and charging the broadcaster's wallet for it would bill them for our own
    /// rebalancing.
    pub fn preroll_for_migration(
        node_id: mm_core::fleet::NodeId,
        viewer_id: String,
        stream_source_id: String,
        ad_source_id: String,
    ) -> Self {
        Self {
            viewer_id,
            stream_source_id,
            ad_source_id,
            started_at: std::time::Instant::now(),
            node_id: Some(node_id),
            slot: mm_ads::creative::AdSlot::PreRoll,
            billable: false,
        }
    }
}

/// Type alias for the shared state passed to handlers via `axum::extract::State`.
pub type SharedState = Arc<AppState>;
