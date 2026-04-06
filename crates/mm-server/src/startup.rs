use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::info;

use mm_api::middleware::AuthConfig;
use mm_api::state::AppState;
use mm_core::cache::TokenCache;
use mm_core::config::Config;
use mm_core::media::{CdnStorage, LocalStorage, MediaStorage};
use mm_core::metrics::Metrics;
use mm_db::Database;
use mm_db::sqlite::SqliteDatabase;
use mm_matrix::appservice::AppserviceHandler;
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
    // 1. Database
    // ---------------------------------------------------------------
    let db_path = &config.database.path;
    // Ensure parent directory exists.
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let db_url = format!("sqlite:{db_path}?mode=rwc");
    let db = SqliteDatabase::new(&db_url).await?;
    db.migrate().await?;
    info!("Database initialized at {db_path}");

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
    // 6. Appservice handler
    // ---------------------------------------------------------------
    let appservice_handler = AppserviceHandler::new(hs_client.clone());

    // ---------------------------------------------------------------
    // 7. Prometheus metrics
    // ---------------------------------------------------------------
    let metrics = Metrics::new();

    // ---------------------------------------------------------------
    // 8. Build shared AppState
    // ---------------------------------------------------------------
    let shared_state = Arc::new(AppState {
        db: Box::new(db),
        sfu: Box::new(sfu),
        hs_client,
        token_cache,
        config: config.clone(),
        appservice_handler,
        metrics,
        started_at: std::time::Instant::now(),
    });

    // ---------------------------------------------------------------
    // 9. Auth config for middleware extractors
    // ---------------------------------------------------------------
    let auth_config = AuthConfig {
        jwt_signing_key: config.jwt_signing_key.clone(),
        admin_token: config.server.admin_token.clone(),
        hs_token: config.matrix.hs_token.clone(),
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
