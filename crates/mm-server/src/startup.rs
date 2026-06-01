use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::info;

use mm_api::middleware::AuthConfig;
use mm_api::state::AppState;
use mm_core::cache::{RedisCache, TokenCache};
use mm_core::config::Config;
use mm_core::media::{CdnStorage, LocalStorage, MediaStorage};
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_db::PgDatabase;
use mm_matrix::appservice::AppserviceHandler;
use mm_payment::EntitlementService;
use mm_payment::PaymentProviderRegistry;
use mm_payment::mock::MockProvider;
use mm_payment::stripe::StripeProvider;
use mm_sfu::livekit::LiveKitAdapter;
use mm_sfu::{CircuitBreakerAdapter, SfuAdapter};

/// Wire up all services and start the HTTP servers.
///
/// Returns when the cancellation token is triggered (graceful shutdown).
pub async fn run(
    config: Config,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let client_bind = config.server.client_bind.clone();
    let admin_bind = config.server.admin_bind.clone();
    let metrics_port = config.server.metrics_port;
    let widget_dir = config.server.widget_dir.clone();
    let cors_origins = config.server.cors_origins.clone();

    // ---------------------------------------------------------------
    // 1. Database (PostgreSQL -- single DB for all tables)
    // ---------------------------------------------------------------
    let pg_url = &config.database.url;
    let db = PgDatabase::new(pg_url).await?;
    db.migrate().await?;
    info!("PostgreSQL database initialized");

    // ---------------------------------------------------------------
    // 2. Homeserver client
    // ---------------------------------------------------------------
    let bot_user_id = format!(
        "@{}:{}",
        config.matrix.bot_localpart, config.matrix.server_name
    );
    let hs_client = mm_matrix::client::HomeserverClient::new(
        config.matrix.homeserver_url.clone(),
        config.matrix.as_token.clone(),
        bot_user_id,
    );

    // ---------------------------------------------------------------
    // 3. SFU adapter (LiveKit + circuit breaker)
    // ---------------------------------------------------------------
    let livekit_url = config
        .sfu
        .livekit_url
        .clone()
        .unwrap_or_else(|| "http://localhost:7880".to_string());
    let lk_adapter = LiveKitAdapter::new(
        livekit_url,
        config.sfu.livekit_api_key.clone(),
        config.sfu.livekit_api_secret.clone(),
    );
    let sfu = CircuitBreakerAdapter::new(lk_adapter);
    info!("SFU adapter: {}", sfu.name());

    // ---------------------------------------------------------------
    // 4. Token cache
    // ---------------------------------------------------------------
    let token_cache = TokenCache::default();

    // ---------------------------------------------------------------
    // 5. Media storage backend
    // ---------------------------------------------------------------
    let storage: Box<dyn MediaStorage> = match config.storage.backend.as_str() {
        "s3" => {
            #[cfg(feature = "s3")]
            {
                use mm_core::media::S3Storage;
                let s3 = S3Storage::new(&config.storage.s3)
                    .await
                    .map_err(|e| format!("S3 storage init failed: {e}"))?;
                info!("Storage backend: S3 (bucket={})", config.storage.s3.bucket);
                if config.cdn.enabled {
                    info!(
                        "CDN enabled: {} (TTL={}s)",
                        config.cdn.base_url, config.cdn.default_ttl_secs
                    );
                    Box::new(CdnStorage::new(
                        s3,
                        config.cdn.base_url.clone(),
                        config.cdn.signing_key.clone(),
                        Duration::from_secs(config.cdn.default_ttl_secs),
                    ))
                } else {
                    Box::new(s3)
                }
            }
            #[cfg(not(feature = "s3"))]
            {
                return Err("S3 storage backend requested but not compiled in. \
                            Build with --features s3"
                    .into());
            }
        }
        _ => {
            info!("Storage backend: local ({})", config.storage.local_path);
            let local = LocalStorage::new(&config.storage.local_path);
            if config.cdn.enabled {
                info!(
                    "CDN enabled: {} (TTL={}s)",
                    config.cdn.base_url, config.cdn.default_ttl_secs
                );
                Box::new(CdnStorage::new(
                    local,
                    config.cdn.base_url.clone(),
                    config.cdn.signing_key.clone(),
                    Duration::from_secs(config.cdn.default_ttl_secs),
                ))
            } else {
                Box::new(local)
            }
        }
    };
    let _storage = storage; // stored for later use when media API routes are added

    // ---------------------------------------------------------------
    // 6. Appservice handler — wired with the always-present signup pool
    //    so the newsfeed indexer fan-out has a Postgres connection. The
    //    `signup_pool` (cloned a few lines below) is the right pool here
    //    because it exists regardless of monetization enablement.
    // ---------------------------------------------------------------
    let appservice_handler = AppserviceHandler::with_feed_indexer(
        hs_client.clone(),
        db.pool().clone(),
        config.matrix.server_name.clone(),
    );

    // ---------------------------------------------------------------
    // 7. Prometheus metrics
    // ---------------------------------------------------------------
    let metrics = Metrics::new();

    // ---------------------------------------------------------------
    // 7b. Redis cache (optional -- shared L2 cache across instances)
    // ---------------------------------------------------------------
    let redis_cache: Option<Arc<RedisCache>> = if !config.monetization.redis_url.is_empty() {
        match RedisCache::new(&config.monetization.redis_url).await {
            Ok(r) => {
                info!("Redis cache connected ({})", config.monetization.redis_url);
                Some(Arc::new(r))
            }
            Err(e) => {
                tracing::warn!("Redis connection failed, falling back to moka: {e}");
                None
            }
        }
    } else {
        info!("No MM_REDIS_URL configured -- using moka-only caching");
        None
    };

    // ---------------------------------------------------------------
    // 7c. Monetization: Stripe (conditional)
    // ---------------------------------------------------------------
    // The PG pool is shared from PgDatabase -- no separate connection needed.
    let signup_pool = db.pool().clone();
    let pg_pool_clone = db.pool().clone();
    let (pg_pool, stripe_client, payment_registry, entitlement_service) =
        if config.monetization.enabled {
            config
                .monetization
                .validate()
                .map_err(|e| format!("Monetization config: {e}"))?;

            // Security guard: block MockProvider keys in release builds unless
            // explicitly overridden via MM_ALLOW_MOCK=true.
            if !cfg!(debug_assertions)
                && config
                    .monetization
                    .stripe_secret_key
                    .starts_with("sk_test_mock")
            {
                let allow_mock = std::env::var("MM_ALLOW_MOCK").unwrap_or_default() == "true";
                if !allow_mock {
                    return Err("MockProvider keys not allowed in release builds. \
                         Set MM_ALLOW_MOCK=true to override"
                        .into());
                }
                tracing::warn!("MockProvider keys allowed in release build via MM_ALLOW_MOCK=true");
            }

            info!("Monetization enabled -- using shared PG pool");

            // Create Stripe client (honoring MM_STRIPE_API_BASE for test/fake servers)
            let stripe_api_base = config.monetization.stripe_api_base.as_str();
            let stripe =
                stripe::Client::from_url(stripe_api_base, &config.monetization.stripe_secret_key);
            info!("Stripe client initialized (api_base={stripe_api_base})");

            // Build payment provider registry
            let mut registry = PaymentProviderRegistry::new();
            if config
                .monetization
                .stripe_secret_key
                .starts_with("sk_test_mock")
                || config.monetization.stripe_secret_key.is_empty()
            {
                info!("Using MockProvider as 'stripe' (no real Stripe key configured)");
                registry.register(Arc::new(MockProvider::with_name("stripe")));
            } else {
                let stripe_provider = Arc::new(StripeProvider::with_api_base(
                    stripe_api_base,
                    &config.monetization.stripe_secret_key,
                    &config.monetization.webhook_signing_secret,
                ));
                registry.register(stripe_provider);
            }
            // Register LNBits provider if configured
            if config.monetization.lnbits_enabled && !config.monetization.lnbits_url.is_empty() {
                let webhook_url = config.server.public_url.as_ref()
                    .map(|u| format!("{u}/_mm/webhooks/lnbits"));
                let lnbits_provider = Arc::new(mm_payment::lnbits::LNBitsProvider::new(
                    &config.monetization.lnbits_url,
                    &config.monetization.lnbits_invoice_key,
                    &config.monetization.lnbits_admin_key,
                    webhook_url.as_deref(),
                ));
                registry.register(lnbits_provider);
                info!("Lightning payments enabled via LNBits: {}", config.monetization.lnbits_url);
            }

            info!("Payment registry: {:?}", registry.available_providers());

            // Initialize entitlement service when subscriptions are enabled.
            let ent_service = if config.monetization.subscriptions_enabled {
                info!("Entitlement service initialized (subscriptions enabled)");
                Some(Arc::new(EntitlementService::new(
                    pg_pool_clone.clone(),
                    redis_cache.clone(),
                    Some(metrics.redis_fallback_total.clone()),
                )))
            } else {
                None
            };

            (
                Some(pg_pool_clone),
                Some(stripe),
                Some(Arc::new(registry)),
                ent_service,
            )
        } else {
            info!("Monetization disabled -- skipping Stripe init");
            (None, None, None, None)
        };

    // ---------------------------------------------------------------
    // 8. Build shared AppState
    // ---------------------------------------------------------------
    // ---------------------------------------------------------------
    // 8a. Advertising engine (Phase 9)
    // ---------------------------------------------------------------
    let ad_engine = if config.advertising.enabled {
        if let Some(ref pool) = pg_pool {
            let public_url = config.server.public_url.clone().unwrap_or_default();
            let engine = mm_ads::AdDecisionEngine::new(
                config.advertising.clone(),
                pool.clone(),
                public_url,
            );
            info!("Advertising engine initialized");
            Some(Arc::new(engine))
        } else {
            tracing::warn!("Advertising enabled but no PG pool -- skipping ad engine");
            None
        }
    } else {
        None
    };

    // ---------------------------------------------------------------
    // 8b. Media switch client (Phase 9 — ad injection via WebRTC switching)
    // ---------------------------------------------------------------
    let switch_auth_secret = std::env::var("MM_SWITCH_AUTH_SECRET").ok().filter(|s| !s.is_empty());
    if switch_auth_secret.is_some() {
        info!("mm-switch auth: HMAC token signing enabled");
    }

    let switch_client = if !config.advertising.switch_url.is_empty() {
        let client = if let Some(ref secret) = switch_auth_secret {
            mm_core::switch_client::SwitchClient::with_auth(
                &config.advertising.switch_url,
                secret.clone(),
            )
        } else {
            mm_core::switch_client::SwitchClient::new(&config.advertising.switch_url)
        };
        info!("Media switch client: {}", config.advertising.switch_url);
        Some(Arc::new(client))
    } else {
        None
    };

    let shared_state = Arc::new(AppState {
        db: Box::new(db),
        sfu: Box::new(sfu),
        hs_client,
        token_cache,
        config: config.clone(),
        appservice_handler,
        metrics,
        started_at: std::time::Instant::now(),
        pg_pool,
        stripe_client,
        payment_registry,
        lnurl_client: mm_payment::lnurl::LnurlPayClient::new(),
        entitlement_service,
        redis: redis_cache,
        ad_engine,
        switch_client,
        switch_auth_secret,
        ad_switches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        signup_limiter: mm_api::rate_limit::SignupRateLimiter::new(
            config.matrix.signup_rate_limit_per_ip_per_hour,
        ),
        signup_avail_limiter: mm_api::rate_limit::SignupRateLimiter::new(60),
        synapse_admin: std::sync::Arc::new(mm_core::synapse_admin::SynapseAdminClient::new(
            &config.matrix.homeserver_url,
            config.matrix.synapse_registration_secret.clone(),
        )),
        signup_pool,
        announcement_cache: Arc::new(
            moka::future::Cache::builder()
                .max_capacity(1)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
        ),
        feed_cache: Arc::new(
            moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
        ),
        feed_limiter: mm_api::rate_limit::SignupRateLimiter::new(1800),
    });

    // ---------------------------------------------------------------
    // 9. Auth config for middleware extractors
    // ---------------------------------------------------------------
    let auth_config = AuthConfig {
        jwt_signing_key: config.jwt_signing_key.clone(),
        admin_token: config.server.admin_token.clone(),
        hs_token: config.matrix.hs_token.clone(),
        matrix_homeserver_url: config.matrix.homeserver_url.clone(),
    };

    // ---------------------------------------------------------------
    // 10. Build routers
    // ---------------------------------------------------------------
    let mut client_app = mm_api::client_router(shared_state.clone())
        .merge(mm_api::wellknown::routes(shared_state.clone()));

    // Optionally serve the built widget static files at /_mm/widget/.
    if let Some(ref dir) = widget_dir {
        info!("Serving widget static files from {dir} at /_mm/widget/");
        client_app = client_app.merge(mm_api::widget_static_router(dir));
    }

    let client_router =
        mm_api::middleware::apply_middleware(client_app, &cors_origins, auth_config.clone());
    let admin_router = mm_api::middleware::apply_middleware(
        mm_api::admin_router(shared_state.clone()),
        &cors_origins,
        auth_config,
    );

    let metrics_router = mm_api::metrics_router(shared_state.clone());

    // ---------------------------------------------------------------
    // 11. Bind listeners
    // ---------------------------------------------------------------

    // Bind client API.
    let client_listener = tokio::net::TcpListener::bind(&client_bind).await?;
    info!("Client API listening on {client_bind}");

    // Bind admin API.
    let admin_listener = tokio::net::TcpListener::bind(&admin_bind).await?;
    info!("Admin API listening on {admin_bind}");

    // Bind metrics API.
    let metrics_addr = SocketAddr::from(([0, 0, 0, 0], metrics_port));
    let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;
    info!("Metrics endpoint listening on {metrics_addr}");

    // ---------------------------------------------------------------
    // 12. Spawn servers
    // ---------------------------------------------------------------

    let cancel_clone = cancel.clone();

    // Serve client API.
    let client_handle = tokio::spawn(async move {
        axum::serve(client_listener, client_router)
            .with_graceful_shutdown(cancel_clone.cancelled_owned())
            .await
            .expect("client server failed");
    });

    let cancel_clone = cancel.clone();

    // Serve admin API.
    let admin_handle = tokio::spawn(async move {
        axum::serve(admin_listener, admin_router)
            .with_graceful_shutdown(cancel_clone.cancelled_owned())
            .await
            .expect("admin server failed");
    });

    let cancel_clone = cancel.clone();

    // Serve metrics API.
    let metrics_handle = tokio::spawn(async move {
        axum::serve(metrics_listener, metrics_router)
            .with_graceful_shutdown(cancel_clone.cancelled_owned())
            .await
            .expect("metrics server failed");
    });

    // SFU health poller — keeps `mm_sfu_health_status` truthful instead of
    // sitting at the IntGauge default of 0 (which Grafana / alerts read as
    // "unhealthy"). Without this loop nothing ever sets the gauge, so the
    // metric is permanently 0 even when LiveKit is fine.
    //
    // 15s tick matches Prometheus' default scrape interval; circuit breaker
    // already debounces/short-circuits failing calls so we don't hammer
    // LiveKit during outages.
    let sfu_poll_state = shared_state.clone();
    let sfu_poll_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = sfu_poll_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let healthy = sfu_poll_state.sfu.health_check().await.is_ok();
                    sfu_poll_state.metrics.sfu_health_status.set(if healthy { 1 } else { 0 });
                }
            }
        }
    });

    // Wait for shutdown signal.
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Shutdown signal received");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C received, initiating graceful shutdown");
            cancel.cancel();
        }
    }

    // Drain period.
    let drain = config.server.drain_seconds;
    info!("Draining for up to {drain}s...");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(drain), async {
        let _ = client_handle.await;
        let _ = admin_handle.await;
        let _ = metrics_handle.await;
    })
    .await;

    info!("Shutdown complete");
    Ok(())
}
