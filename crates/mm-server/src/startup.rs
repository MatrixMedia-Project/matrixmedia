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
    // `mut` for one reason: a misconfigured S1 proxy is forced off below rather
    // than allowed to break every viewer join (FR-346).
    mut config: Config,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let client_bind = config.server.client_bind.clone();
    let admin_bind = config.server.admin_bind.clone();
    let metrics_port = config.server.metrics_port;

    // ---------------------------------------------------------------
    // 1. Database (PostgreSQL -- single DB for all tables)
    // ---------------------------------------------------------------
    let pg_url = &config.database.url;
    let db = PgDatabase::new(pg_url).await?;
    db.migrate().await?;
    info!("PostgreSQL database initialized");

    // ---------------------------------------------------------------
    // 1b. Dashboard settings: import (first boot), overlay, safe mode.
    //     From here on `config` is the EFFECTIVE config (file + env + database);
    //     handlers read the live copy through the settings handle.
    // ---------------------------------------------------------------
    let (keys, key_error) = match mm_core::settings::crypto::KeyRing::from_env() {
        Ok(keys) => (keys, None),
        Err(e) => {
            tracing::error!(error = %e, "settings: encryption key unusable — secrets stay file/env-sourced");
            (None, Some(e))
        }
    };
    let settings = mm_api::settings_service::SettingsService::boot(
        db.pool().clone(),
        config,
        keys,
        mm_api::settings_service::BootOptions { key_error, ..mm_api::settings_service::BootOptions::production() },
        cancel.clone(),
    )
    .await?;
    let config: Config = (*settings.handle().load()).clone();
    let config_handle = settings.handle().clone();
    let widget_dir = config.server.widget_dir.clone();
    if config.server.cors_origins.is_empty() {
        tracing::warn!(
            "No MM_CORS_ORIGINS configured, using localhost defaults. Set explicit origins for production."
        );
    }
    // Name the alert webhook's mode once: "Config override: MM_ALERT_WEBHOOK_TOKEN" is
    // also logged for an empty value, so it cannot confirm a token took effect.
    if config.server.alert_webhook_token.is_empty() {
        tracing::warn!(
            "alert webhook: MM_ALERT_WEBHOOK_TOKEN not set; /_mm/internal/alert-webhook accepts any request \
             that reaches mm-core directly from a private address. Set it wherever something NATs traffic \
             to mm-core's port (deploy/README.md, Observability)."
        );
    } else {
        tracing::info!("alert webhook: bearer token required");
    }

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
        config.sfu.livekit_public_url.clone(),
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
    //     The connect gives up after mm_core::cache::REDIS_CONNECT_TIMEOUT, so an
    //     unreachable redis_url cannot hold up the API and the dashboard. Never log the
    //     URL: it may carry a password.
    // ---------------------------------------------------------------
    let redis_cache: Option<Arc<RedisCache>> = if !config.monetization.redis_url.is_empty() {
        match RedisCache::new(&config.monetization.redis_url).await {
            Ok(r) => {
                info!("Redis cache connected");
                Some(Arc::new(r))
            }
            Err(e) => {
                tracing::warn!(error = %e, "Redis unavailable, falling back to moka");
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
            // Last check before the keys are used, with the same rules and build policy
            // (release build, MM_ALLOW_MOCK) the settings overlay applied: a stored value
            // that breaks them already put the server in safe mode, so only a file/env
            // value can still fail here — e.g. a mock Stripe key in a release build.
            let policy = settings.build_policy();
            config
                .monetization
                .validate_for(policy)
                .map_err(|e| format!("Monetization config: {e}"))?;
            if policy.release_build
                && config
                    .monetization
                    .stripe_secret_key
                    .starts_with(mm_core::config::MOCK_STRIPE_KEY_PREFIX)
            {
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
                .starts_with(mm_core::config::MOCK_STRIPE_KEY_PREFIX)
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
    if config.advertising.switch_auth_secret_opt().is_some() {
        info!("mm-switch auth: HMAC token signing enabled");
    }
    if config.turn.shared_secret_opt().is_some() {
        info!(
            "TURN ephemeral credentials enabled (ttl={}s, {} url(s))",
            config.turn.ttl_secs,
            config.turn.urls.len()
        );
    }

    // The configured switch_url is the fleet's ORIGIN. Fleet nodes are added to
    // the pool at runtime by the fleet runner; with none registered the pool
    // resolves everything to this client, which is byte-for-byte the behaviour
    // before the pool existed (FR-102).
    // FR-346 needs a secret: the proxy mints a per-request viewer-role token, and
    // forwarding an UNBOUND offer would place a viewer on a node with no identity
    // at all. Rather than refuse to boot — mm-core is the whole service, and taking
    // it down is worse than not proxying — the proxy stays off and this says so.
    // The rule itself is `switch_proxy::proxy_enabled`, applied where the join
    // reads the config: `config` here is a copy, so clearing a flag on it would
    // change nothing a handler sees.
    if config.fleet.proxy_viewers && !mm_api::switch_proxy::proxy_enabled(&config) {
        tracing::error!(
            "fleet.proxy_viewers is on but MM_SWITCH_AUTH_SECRET is unset — the S1 \
             proxy cannot mint a bound viewer token, so it stays OFF and clients \
             will keep talking to the switch directly. Set the secret to enable it."
        );
    }
    if mm_api::switch_proxy::proxy_enabled(&config) {
        info!("S1 viewer proxy: ENABLED — clients will signal through mm-core (FR-346)");
    }

    let switch_pool = if !config.advertising.switch_url.is_empty() {
        let client = match config.advertising.switch_auth_secret_opt() {
            Some(secret) => mm_core::switch_client::SwitchClient::with_auth(
                &config.advertising.switch_url,
                secret.to_string(),
            ),
            None => mm_core::switch_client::SwitchClient::new(&config.advertising.switch_url),
        };
        info!(
            "Media switch origin: {} (fleet mode: {})",
            config.advertising.switch_url, config.fleet.mode
        );
        Some(Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
            client,
        ))))
    } else {
        None
    };

    // Built once, here. The discovery handlers used to construct one per request, which
    // meant the engine's 5-minute cache started cold every single time — so the
    // unauthenticated trending endpoint ran a full recalculation on every hit.
    let trending_engine = pg_pool
        .clone()
        .map(|pool| Arc::new(mm_recommendations::trending::TrendingEngine::new(pool)));

    let shared_state = Arc::new(AppState {
        db: Box::new(db),
        sfu: Box::new(sfu),
        hs_client,
        token_cache,
        config_handle: config_handle.clone(),
        settings: settings.clone(),
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
        switch_pool,
        broadcast_servers: Arc::new(mm_api::broadcast_servers::SnapshotCell::new()),
        ad_switches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        signup_limiter: mm_api::rate_limit::LiveQuotaLimiter::new(
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
        moderation_report_limiter: mm_api::rate_limit::SignupRateLimiter::new(10),
        permissions_cache: Arc::new(mm_api::middleware::tier_gate::new_permissions_cache()),
        trending_engine,
    });

    // Re-attach MP4 transcode pollers to rows orphaned by a restart
    // (one-shot sweep; see mm_api::mp4_tracker).
    // Recordings live on the origin, so the tracker polls the origin.
    if let (Some(pool), Some(switch)) = (
        shared_state.pg_pool.clone(),
        shared_state.origin_switch(),
    ) {
        tokio::spawn(mm_api::mp4_tracker::resume_pending(pool, switch));
    }

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
        .merge(mm_api::wellknown::routes(shared_state.clone()))
        // Unauthenticated K8s readiness probe (DB-backed). Liveness should
        // use a static route instead — see mm_api::readyz.
        .merge(mm_api::readyz::routes(shared_state.clone()));

    // Optionally serve the built widget static files at /_mm/widget/.
    if let Some(dir) = widget_dir.as_deref().filter(|d| !d.is_empty()) {
        info!("Serving widget static files from {dir} at /_mm/widget/");
        client_app = client_app.merge(mm_api::widget_static_router(dir));
    }

    let client_router = mm_api::middleware::apply_middleware(
        client_app,
        config_handle.clone(),
        auth_config.clone(),
    );
    let admin_router = mm_api::middleware::apply_middleware(
        mm_api::admin_router(shared_state.clone()),
        config_handle.clone(),
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

    // Serve client API. With the peer address: the alert webhook trusts a
    // tokenless request only when it comes straight from a private address.
    let client_handle = tokio::spawn(async move {
        axum::serve(
            client_listener,
            client_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
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
    supervise("sfu_health_poll", cancel.clone(), move || {
        let sfu_poll_state = sfu_poll_state.clone();
        let sfu_poll_cancel = sfu_poll_cancel.clone();
        async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = sfu_poll_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let healthy = sfu_poll_state.sfu.health_check().await.is_ok();
                    sfu_poll_state.metrics.sfu_health_status.set(if healthy { 1 } else { 0 });
                    mm_core::metrics_global::heartbeat("sfu_health_poll");
                }
            }
        }
        }
    });

    // E3 moderation: pull Synapse event/room reports into the MM moderation
    // queue every 60s so operators see Matrix-origin reports alongside
    // MM-native ones. Skips cleanly when no Synapse admin token is configured;
    // failures log and retry next tick (never crash the loop).
    let mod_sync_state = shared_state.clone();
    let mod_sync_cancel = cancel.clone();
    supervise("moderation_sync", cancel.clone(), move || {
        let mod_sync_state = mod_sync_state.clone();
        let mod_sync_cancel = mod_sync_cancel.clone();
        async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = mod_sync_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if mod_sync_state.config().matrix.synapse_admin_token.is_empty() {
                        continue;
                    }
                    match mm_api::moderation::run_sync(&mod_sync_state).await {
                        Ok(n) if n > 0 => info!(ingested = n, "moderation: synced Synapse reports"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e.0, "moderation: report sync failed"),
                    }
                    // Beats on a failed sync too: the loop is alive and retrying, which is
                    // a different condition from the loop being gone. Sync failures have
                    // their own signal.
                    mm_core::metrics_global::heartbeat("moderation_sync");
                }
            }
        }
        }
    });

    // Stream liveness sweep: every 60s, auto-end streams that are not live (no
    // active mm-switch WebRTC publisher and no LiveKit participants) for longer
    // than `streaming.auto_end_grace_secs` (default 600s), and streams that
    // started more than `streaming.max_broadcast_secs` ago (default 12h), live
    // or not. Both go through the host-end path (`end_and_finalise_stream`):
    // recordings finalised, switch source removed, terminal marker written.
    // The generous grace window protects the host resume flow
    // (POST /streams/{id}/resume): a briefly-disconnected host must never
    // have their broadcast killed mid-reconnect. Spawned unconditionally:
    // `run_stream_sweep` reads both settings from the live config every tick,
    // so a change (including toggling either to/from 0, which turns that rule
    // off) applies without a restart.
    info!(
        "Stream liveness sweep: tick 60s, grace from streaming.auto_end_grace_secs (currently {}s; 0 = off), cap from streaming.max_broadcast_secs (currently {}s; 0 = no limit)",
        config.streaming.auto_end_grace_secs,
        config.streaming.max_broadcast_secs
    );
    {
        let sweep_state = shared_state.clone();
        let sweep_cancel = cancel.clone();
        supervise("stream_sweep", cancel.clone(), move || {
            let sweep_state = sweep_state.clone();
            let sweep_cancel = sweep_cancel.clone();
            async move {
            let mut sweeper = mm_api::stream_lifecycle::StreamSweeper::new();
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = sweep_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        let report =
                            mm_api::stream_lifecycle::run_stream_sweep(&sweep_state, &mut sweeper)
                                .await;
                        if !report.ended.is_empty() {
                            info!(
                                ended = report.ended.len(),
                                over_max_duration = report.over_max_duration.len(),
                                checked = report.checked,
                                marker_failures = report.marker_failures,
                                "stream sweep: auto-ended streams"
                            );
                        }
                        mm_core::metrics_global::heartbeat("stream_sweep");
                    }
                }
            }
        }
        });
    }

    // Broadcast servers collector: every 10 s, observe mm-switch, LiveKit and the active
    // streams and cache one snapshot for GET /_mm/admin/v1/broadcast-servers, so a page
    // load never probes a server. Reads the config each tick (Live settings apply).
    {
        let bs_state = shared_state.clone();
        let bs_cancel = cancel.clone();
        supervise("broadcast_servers", cancel.clone(), move || {
            let bs_state = bs_state.clone();
            let bs_cancel = bs_cancel.clone();
            async move {
                let mut trackers = mm_api::broadcast_servers::Trackers::default();
                let mut ticker = tokio::time::interval(Duration::from_secs(
                    mm_api::broadcast_servers::COLLECT_INTERVAL_SECS,
                ));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = bs_cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            // Race the probes against shutdown: a hung switch or SFU call
                            // must not hold the process open until its own timeout.
                            tokio::select! {
                                _ = bs_cancel.cancelled() => break,
                                _ = mm_api::broadcast_servers::collect_tick(&bs_state, &mut trackers) => {}
                            }
                            mm_core::metrics_global::heartbeat("broadcast_servers");
                        }
                    }
                }
            }
        });
    }

    // Settings revision poll (spec §5.7): picks up Live changes saved on other
    // instances and restarts this one when "Apply & restart" asks for it.
    let poll_settings = settings.clone();
    let poll_cancel = cancel.clone();
    supervise("settings_poll", cancel.clone(), move || {
        let poll_settings = poll_settings.clone();
        let poll_cancel = poll_cancel.clone();
        async move {
            let mut ticker = tokio::time::interval(poll_settings.poll_interval());
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = poll_cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(e) = poll_settings.poll_once().await {
                            tracing::warn!(error = %e, "settings: revision poll failed");
                        }
                        mm_core::metrics_global::heartbeat("settings_poll");
                    }
                }
            }
        }
    });

    // Egress metering, and — only if an operator switched it on — billing.
    //
    // Two conditions, both hard: a switch to poll, and Postgres to write to. Usage
    // events, baselines and wallets are all Postgres-only; on SQLite there is
    // nothing to meter into, and starting a loop that fails every 60s would be a
    // log full of noise standing in for a feature that is not available.
    let meter_secs = config.fleet.meter_interval_secs;
    // Read back from AppState: both were moved into it above.
    match (
        meter_secs > 0,
        shared_state.switch_pool.as_ref(),
        shared_state.pg_pool.as_ref(),
    ) {
        (true, Some(pool), Some(pg)) => {
            let billing = config.fleet.billing_enabled;
            let batch = config.fleet.rating_batch;
            if billing {
                info!(
                    "Egress meter ENABLED (tick {meter_secs}s) and BILLING IS ON —                      wallets will be charged, batch {batch}"
                );
            } else {
                info!(
                    "Egress meter enabled (tick {meter_secs}s); billing is OFF, so                      usage accumulates unrated (MM_BILLING_ENABLED=true to charge)"
                );
            }
            let meter_pool = pool.clone();
            let meter_db = mm_db::metering_db::PgMeteringDb::new(pg.clone());
            let meter_wallet = mm_db::wallet_db::PgWalletDb::new(pg.clone());
            let meter_cancel = cancel.clone();
            supervise("egress_meter", cancel.clone(), move || {
                let switch_pool = meter_pool.clone();
                let meter_db = meter_db.clone();
                let meter_wallet = meter_wallet.clone();
                let meter_cancel = meter_cancel.clone();
                async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(meter_secs));
                    // Delay, not Burst: a tick missed because the previous sweep ran
                    // long must not be made up by firing several immediately. Each
                    // sweep reads every node's counters and writes usage, and a burst
                    // of them would pile transactions on top of the slow thing that
                    // caused the delay.
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    // The backlog at the last "billing is off" warning, so it fires
                    // on doubling rather than every tick (see backlog_warning_due).
                    // Local to the loop body: a supervised restart says it once
                    // more, which is right — a restart is worth re-stating it.
                    let mut last_warned: Option<i64> = None;
                    loop {
                        tokio::select! {
                            _ = meter_cancel.cancelled() => break,
                            _ = ticker.tick() => {
                                // Rebuilt every tick, not captured once: nodes are
                                // provisioned and reaped while this loop runs, and a
                                // stale list would keep polling a destroyed node
                                // (counted as unreachable) and never poll a new one
                                // (its egress unbilled).
                                let nodes = metered_nodes(&switch_pool).await;
                                let tick = mm_fleet::meter_loop::meter_tick(
                                    &meter_db,
                                    &meter_wallet,
                                    &nodes,
                                    billing,
                                    batch,
                                    chrono::Utc::now(),
                                )
                                .await;
                                mm_fleet::meter_loop::publish(&tick, billing);
                                last_warned =
                                    mm_fleet::meter_loop::log_tick(&tick, billing, last_warned);
                                // Unconditional, like every other supervised loop:
                                // "the loop is alive" is a different condition from
                                // "the sweep succeeded", and the sweep has its own
                                // signals.
                                mm_core::metrics_global::heartbeat("egress_meter");
                            }
                        }
                    }
                }
            });
        }
        (false, _, _) => info!("Egress meter disabled (fleet.meter_interval_secs = 0)"),
        (true, None, _) => {
            info!("Egress meter disabled: no switch configured, so there is nothing to poll")
        }
        (true, Some(_), None) => tracing::warn!(
            "Egress meter disabled: it needs Postgres (usage events, baselines and \
             wallets are Postgres-only) and this instance has none. Egress is NOT \
             being metered."
        ),
    }

    // The demotion ladder (design §17.4). Evaluates every live broadcast against its
    // wallet and — depending on the mode — degrades or ends it.
    //
    // The default mode changes nothing, and that is load-bearing rather than
    // cautious: with no rate card and no funded wallets, every broadcast computes a
    // zero balance, which is the ladder's `end_with_slate`. Shipping this switched on
    // would end every live broadcast on the platform. `observe` still runs, because a
    // record of what the ladder WOULD have done to real broadcasts is how the
    // placeholder watermarks get set.
    let ladder_secs = config.fleet.ladder_interval_secs;
    match (ladder_secs > 0, shared_state.pg_pool.as_ref()) {
        (true, Some(pg)) => {
            let mode = config.fleet.ladder_mode;
            match mode {
                mm_fleet::ladder_loop::LadderMode::Observe => info!(
                    "Demotion ladder: OBSERVE (tick {ladder_secs}s) — rungs are                      recorded and nothing is applied"
                ),
                mm_fleet::ladder_loop::LadderMode::Degrade => tracing::warn!(
                    "Demotion ladder: DEGRADE (tick {ladder_secs}s) — low-balance                      broadcasts WILL have recording stopped and viewers drained.                      Broadcasts are never ended in this mode."
                ),
                mm_fleet::ladder_loop::LadderMode::Full => tracing::warn!(
                    "Demotion ladder: FULL (tick {ladder_secs}s) — low-balance                      broadcasts WILL BE ENDED. Check mm_billing_unrated_events and                      mm_broadcast_demotion before leaving this on."
                ),
            }

            let ladder_db = mm_db::ladder_db::PgLadderDb::new(pg.clone());
            // The ladder's own quote, not the planner's. The planner asks "can this
            // wallet afford one more node?"; the ladder asks "can it afford what is
            // running?" — actual nodes, measured egress, and usage already owed.
            let ladder_billing: Arc<dyn mm_fleet::runner::BillingSource> =
                Arc::new(mm_fleet::ladder_billing::LadderBillingSource::new(
                    pg.clone(),
                    config.fleet.wallet_currency.clone(),
                ));
            let ladder_actuator: Arc<dyn mm_fleet::ladder_loop::LadderActuator> = Arc::new(
                mm_api::ladder_actuator::StateLadderActuator::new(shared_state.clone()),
            );
            let ladder_batch = config.fleet.ladder_batch;
            let ladder_policy = mm_core::fleet::ladder::LadderPolicy::placeholder();
            let ladder_cancel = cancel.clone();
            supervise("demotion_ladder", cancel.clone(), move || {
                let ladder_db = ladder_db.clone();
                let ladder_billing = ladder_billing.clone();
                let ladder_actuator = ladder_actuator.clone();
                let ladder_cancel = ladder_cancel.clone();
                async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(ladder_secs));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tokio::select! {
                            _ = ladder_cancel.cancelled() => break,
                            _ = ticker.tick() => {
                                match mm_fleet::ladder_loop::ladder_tick(
                                    &ladder_db,
                                    ladder_billing.as_ref(),
                                    ladder_actuator.as_ref(),
                                    mode,
                                    &ladder_policy,
                                    ladder_batch,
                                    chrono::Utc::now(),
                                )
                                .await
                                {
                                    Ok(report) => {
                                        mm_fleet::ladder_loop::log_tick(&report, mode);
                                    }
                                    Err(e) => tracing::error!("demotion ladder: {e}"),
                                }
                                mm_fleet::ladder_loop::publish(&ladder_db).await;
                                mm_core::metrics_global::heartbeat("demotion_ladder");
                            }
                        }
                    }
                }
            });
        }
        (false, _) => info!("Demotion ladder disabled (fleet.ladder_interval_secs = 0)"),
        (true, None) => tracing::warn!(
            "Demotion ladder disabled: it needs Postgres (wallets and ladder state are \
             Postgres-only). A broadcast whose balance runs out will NOT be degraded."
        ),
    }

    // Wait for shutdown signal.
    //
    // SIGTERM matters more than SIGINT here: `docker stop` (and every orchestrator)
    // sends SIGTERM, and Rust's default action for it is to terminate the process
    // immediately. Handling only ctrl_c() meant the drain below NEVER ran in the
    // deployment that actually matters — in-flight requests were cut mid-flight on
    // every deploy.
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Shutdown signal received");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT (Ctrl+C) received, initiating graceful shutdown");
            cancel.cancel();
        }
        _ = terminate_signal() => {
            info!("SIGTERM received, initiating graceful shutdown");
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

/// Resolve when the process receives SIGTERM.
///
/// `docker stop`, Kubernetes, and systemd all use SIGTERM; Rust's default action
/// for it is immediate termination. Without this the graceful-drain path below is
/// dead code in production — it only ever ran for an interactive Ctrl+C.
///
/// On non-Unix targets this never resolves, which correctly leaves ctrl_c() as the
/// only shutdown trigger.
async fn terminate_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler; \
                                            graceful shutdown on SIGTERM is unavailable");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        std::future::pending::<()>().await;
    }
}

/// Every switch the meter should poll: the origin, plus each fleet node.
///
/// The origin is included deliberately. It is `owned` rather than `rented` and costs
/// nothing per hour, but its **egress** is billed like anyone else's, and in `frozen`
/// mode — the default — it is the only node serving viewers. Metering only the rented
/// fleet would meter nothing at all on every installation that has not opted in.
async fn metered_nodes(
    pool: &Arc<mm_api::switch_pool::SwitchPool>,
) -> Vec<mm_fleet::metering::MeteredNode> {
    let mut nodes = vec![mm_fleet::metering::MeteredNode {
        mm_node_id: mm_fleet::metering::ORIGIN_NODE_ID.to_string(),
        client: pool.origin(),
    }];
    for node in pool.nodes().await {
        if let Some(client) = pool.client_for_node(Some(&node.id)).await {
            nodes.push(mm_fleet::metering::MeteredNode {
                mm_node_id: node.id.to_string(),
                client,
            });
        }
    }
    nodes
}

/// Run a background loop under supervision.
///
/// The three long-lived loops (SFU health, moderation sync, stream sweeper) were
/// bare `tokio::spawn`s whose JoinHandles were dropped. A panic inside one killed
/// that task permanently and SILENTLY: no log, no metric, and the server carried on
/// looking healthy while — say — streams stopped being swept. This wrapper catches
/// the panic, logs it loudly, and restarts the loop with a bounded backoff.
///
/// It exits cleanly (without restarting) when the cancellation token fires, so it
/// does not fight the shutdown path.
fn supervise<F, Fut>(
    name: &'static str,
    cancel: tokio_util::sync::CancellationToken,
    mut make_fut: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = std::time::Duration::from_secs(1);
        const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

        loop {
            if cancel.is_cancelled() {
                tracing::info!(task = name, "supervised task stopping (shutdown)");
                return;
            }

            // Run the loop as its own task and await the JoinHandle: a panic
            // surfaces as JoinError::is_panic(). This is the idiomatic tokio way to
            // observe a panic and needs no catch_unwind (and no extra dependency).
            match tokio::spawn(make_fut()).await {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        tracing::info!(task = name, "supervised task finished (shutdown)");
                        return;
                    }
                    mm_core::metrics_global::BACKGROUND_TASK_RESTARTS
                        .with_label_values(&[name, "returned"])
                        .inc();
                    tracing::warn!(
                        task = name,
                        "supervised task returned unexpectedly; restarting"
                    );
                }
                Err(e) if e.is_panic() => {
                    mm_core::metrics_global::BACKGROUND_TASK_RESTARTS
                        .with_label_values(&[name, "panic"])
                        .inc();
                    tracing::error!(
                        task = name,
                        backoff_secs = backoff.as_secs(),
                        "supervised task PANICKED; restarting after backoff"
                    );
                }
                Err(e) => {
                    // Cancelled (runtime shutting down) — nothing to restart into.
                    tracing::info!(task = name, error = %e, "supervised task cancelled");
                    return;
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = cancel.cancelled() => {
                    tracing::info!(task = name, "supervised task stopping during backoff");
                    return;
                }
            }
            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    })
}

#[cfg(test)]
mod supervision_tests {
    use super::supervise;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    /// A panicking loop must be restarted, not silently lost.
    ///
    /// Before this, the three background loops were bare `tokio::spawn`s whose
    /// JoinHandles were dropped: a panic killed the loop permanently with no log
    /// and no metric, while the server kept reporting healthy.
    #[tokio::test(start_paused = true)]
    async fn panicking_task_is_restarted() {
        let runs = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();

        let r = runs.clone();
        let handle = supervise("panicky", cancel.clone(), move || {
            let r = r.clone();
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    panic!("boom #{n}");
                }
                // 4th run: survive until cancelled.
                std::future::pending::<()>().await;
            }
        });

        // start_paused auto-advances time, so the 1s/2s/4s backoffs cost no wall time.
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            while runs.load(Ordering::SeqCst) < 4 {
                tokio::task::yield_now().await;
                tokio::time::advance(std::time::Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("supervisor should have restarted the panicking task");

        assert!(
            runs.load(Ordering::SeqCst) >= 4,
            "expected the task to be restarted after each panic"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    /// Cancellation must stop the supervisor rather than restart-looping forever.
    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_the_supervisor() {
        let cancel = CancellationToken::new();
        let handle = supervise("stopper", cancel.clone(), || async {
            std::future::pending::<()>().await;
        });

        cancel.cancel();

        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
        assert!(
            joined.is_ok(),
            "supervisor should exit promptly once cancelled"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE MOST LIKELY SILENT FAILURE OF THE WHOLE METER. `frozen` is the default
    /// fleet mode, and in `frozen` there are no fleet nodes — every viewer is served
    /// by the origin. A node list built from `pool.nodes()` alone is empty on every
    /// default install, so the meter would run its timer, poll nothing, write nothing,
    /// and report no error at all.
    #[tokio::test]
    async fn the_origin_is_metered_even_with_no_fleet_nodes() {
        let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
            mm_core::switch_client::SwitchClient::new("http://switch.invalid"),
        )));
        assert_eq!(pool.node_count().await, 0, "no fleet nodes, as in `frozen`");

        let nodes = metered_nodes(&pool).await;

        assert_eq!(nodes.len(), 1, "the origin must still be polled");
        assert_eq!(
            nodes[0].mm_node_id,
            mm_fleet::metering::ORIGIN_NODE_ID,
            "and under the stable origin id — a changed id resets the baseline, which \
             silently discards one interval of the busiest node's egress"
        );
    }

    /// Fleet nodes are metered under their own ids, not folded into the origin's:
    /// `mm_egress_baseline` is keyed by node, and two nodes sharing a key would
    /// subtract one's counter from the other's.
    #[tokio::test]
    async fn fleet_nodes_are_metered_under_their_own_ids() {
        let pool = Arc::new(mm_api::switch_pool::SwitchPool::new(Arc::new(
            mm_core::switch_client::SwitchClient::new("http://origin.invalid"),
        )));
        let node = mm_core::fleet::FleetNode {
            id: mm_core::fleet::NodeId::new("fanout-1"),
            flavor: mm_core::fleet::NodeFlavor::Fanout,
            ownership: mm_core::fleet::Ownership::Rented,
            state: mm_core::fleet::NodeState::Healthy,
            viewer_capacity: 100,
            viewers_current: 0,
        };
        pool.upsert(
            &node,
            Arc::new(mm_core::switch_client::SwitchClient::new(
                "http://fanout-1.invalid",
            )),
        )
        .await;

        let ids: Vec<String> = metered_nodes(&pool)
            .await
            .into_iter()
            .map(|n| n.mm_node_id)
            .collect();
        assert_eq!(ids, vec![mm_fleet::metering::ORIGIN_NODE_ID.to_string(), "fanout-1".to_string()]);
    }
}
