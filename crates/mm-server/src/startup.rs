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
        moderation_report_limiter: mm_api::rate_limit::SignupRateLimiter::new(10),
        permissions_cache: Arc::new(
            moka::future::Cache::builder()
                .max_capacity(50_000)
                .time_to_live(std::time::Duration::from_secs(60))
                .build(),
        ),
        trending_engine,
    });

    // Re-attach MP4 transcode pollers to rows orphaned by a restart
    // (one-shot sweep; see mm_api::mp4_tracker).
    if let (Some(pool), Some(switch)) = (
        shared_state.pg_pool.clone(),
        shared_state.switch_client.clone(),
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
                    if mod_sync_state.config.matrix.synapse_admin_token.is_empty() {
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

    // Stream liveness sweep: every 60s, auto-end streams whose SFU room has
    // been empty longer than `streaming.auto_end_grace_secs` (default 600s)
    // and write the terminal `com.matrixmedia.stream` marker through the
    // shared finalize path. The generous grace window protects the host
    // resume flow (POST /streams/{id}/resume): a briefly-disconnected host
    // must never have their broadcast killed mid-reconnect. `0` disables
    // the sweep entirely.
    let sweep_grace_secs = config.streaming.auto_end_grace_secs;
    if sweep_grace_secs > 0 {
        let sweep_state = shared_state.clone();
        let sweep_cancel = cancel.clone();
        info!("Stream liveness sweep enabled (grace {sweep_grace_secs}s, tick 60s)");
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
                                checked = report.checked,
                                marker_failures = report.marker_failures,
                                "stream sweep: auto-ended stale streams"
                            );
                        }
                        mm_core::metrics_global::heartbeat("stream_sweep");
                    }
                }
            }
        }
        });
    } else {
        info!("Stream liveness sweep disabled (streaming.auto_end_grace_secs = 0)");
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
