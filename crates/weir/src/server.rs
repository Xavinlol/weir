use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;

use crate::config::Config;
use crate::health;
use crate::proxy;

#[derive(Clone)]
pub struct AppState {
    pub http_client: reqwest::Client,
    pub config: Arc<Config>,
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

    let state = AppState {
        http_client,
        config: Arc::new(config),
    };

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
