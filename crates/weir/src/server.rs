use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;
use weir_ratelimit::memory::{ManagerConfig, RateLimitManager};

use crate::config::Config;
use crate::health;
use crate::proxy;

#[derive(Clone)]
pub struct AppState {
    pub http_client: reqwest::Client,
    pub config: Arc<Config>,
    pub rate_limiter: Arc<RateLimitManager>,
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .fallback(proxy::handle)
        .with_state(state)
}

pub async fn run(config: Config) -> Result<()> {
    let request_timeout = Duration::from_millis(config.server.request_timeout_ms);

    let http_client = reqwest::Client::builder()
        .timeout(request_timeout)
        .user_agent(format!("Weir/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let overrides = config.ratelimit.overrides
        .iter()
        .map(|(k, v)| (k.clone(), v.global_limit))
        .collect();

    let rate_limiter = Arc::new(RateLimitManager::new(ManagerConfig {
        global_limit_default: config.ratelimit.global_limit_default,
        queue_timeout_ms: config.ratelimit.queue_timeout_ms,
        overrides,
        token_error_threshold: config.protection.consecutive_error_threshold,
        webhook_404_threshold: config.protection.consecutive_404_threshold,
    }));

    let state = AppState {
        http_client,
        rate_limiter,
        config: Arc::new(config),
    };

    let limiter = Arc::clone(&state.rate_limiter);
    let ttl = Duration::from_millis(state.config.ratelimit.bucket_ttl_ms);
    let cleanup_interval = Duration::from_millis(state.config.ratelimit.cleanup_interval_ms);
    tokio::spawn(async move {
        limiter.run_cleanup(cleanup_interval, ttl).await;
    });

    let addr = SocketAddr::new(state.config.server.host, state.config.server.port);
    let listener = TcpListener::bind(addr).await?;

    info!(%addr, "listening");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for Ctrl+C");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received Ctrl+C, shutting down"),
        () = terminate => info!("received SIGTERM, shutting down"),
    }
}
