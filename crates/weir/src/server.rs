use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use metrics::gauge;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::oneshot;
use tracing::{info, warn};
use weir_ratelimit::limiter::Limiter;
use weir_ratelimit::memory::{ManagerConfig, MemoryRateLimiter};

const GAUGE_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(feature = "redis")]
use crate::config::RedisConfig;
use crate::config::{Config, RatelimitBackend};
use crate::health;
use crate::proxy;

#[derive(Clone)]
pub struct AppState {
    pub http_client: reqwest::Client,
    pub config: Arc<Config>,
    pub rate_limiter: Arc<Limiter>,
}

fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .fallback(proxy::handle);

    let router = if state.config.logging.access_log {
        router.layer(middleware::from_fn(access_log))
    } else {
        router
    };

    router.with_state(state)
}

async fn access_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = proxy::redact_path(req.uri().path()).into_owned();
    let start = Instant::now();

    let response = next.run(req).await;

    info!(
        %method,
        %path,
        status = response.status().as_u16(),
        duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        "request"
    );
    response
}

pub async fn run(config: Config) -> Result<()> {
    let request_timeout = Duration::from_millis(config.server.request_timeout_ms);

    if config.metrics.enabled {
        let metrics_addr = SocketAddr::new(config.server.host, config.metrics.port);
        weir_metrics::init(metrics_addr)
            .map_err(|e| anyhow::anyhow!("failed to initialize metrics: {e}"))?;
        info!(%metrics_addr, "metrics endpoint started");
    }

    let http_client = reqwest::Client::builder()
        .timeout(request_timeout)
        .user_agent(format!("Weir/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let rate_limiter = Arc::new(build_limiter(&config).await?);

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

    tokio::spawn(sample_gauges(Arc::clone(&state.rate_limiter)));

    let addr = SocketAddr::new(state.config.server.host, state.config.server.port);
    let shutdown_timeout = Duration::from_millis(state.config.server.graceful_shutdown_timeout_ms);
    let listener = TcpListener::bind(addr).await?;

    info!(%addr, "listening");

    let (signalled_tx, signalled_rx) = oneshot::channel();
    let server = axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = signalled_tx.send(());
        })
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => result?,
        () = drain_deadline(signalled_rx, shutdown_timeout) => {
            warn!(timeout_ms = shutdown_timeout.as_millis(), "graceful shutdown timed out");
        }
    }

    info!("shutdown complete");
    Ok(())
}

async fn drain_deadline(signalled: oneshot::Receiver<()>, timeout: Duration) {
    if signalled.await.is_err() {
        return std::future::pending().await;
    }
    tokio::time::sleep(timeout).await;
}

async fn sample_gauges(limiter: Arc<Limiter>) {
    let mut tick = tokio::time::interval(GAUGE_SAMPLE_INTERVAL);
    loop {
        tick.tick().await;
        let cf = limiter.is_cloudflare_blocked().await;
        let invalid = limiter.invalid_count().await;
        let buckets = limiter.bucket_count();
        gauge!("weir_cloudflare_blocked").set(if cf { 1.0 } else { 0.0 });
        gauge!("weir_invalid_request_count").set(f64::from(invalid));
        #[allow(clippy::cast_precision_loss)]
        gauge!("weir_active_buckets").set(buckets as f64);
    }
}

#[allow(clippy::unused_async)]
async fn build_limiter(config: &Config) -> Result<Limiter> {
    match config.ratelimit.backend {
        RatelimitBackend::Memory => Ok(Limiter::Memory(build_memory_limiter(config))),
        #[cfg(feature = "redis")]
        RatelimitBackend::Redis => {
            let r = weir_ratelimit::redis_backend::RedisRateLimiter::new(redis_config_for(
                &config.redis,
                config,
            ))
            .await?;
            Ok(Limiter::Redis(Box::new(r)))
        }
        #[cfg(not(feature = "redis"))]
        RatelimitBackend::Redis => {
            anyhow::bail!("redis backend selected but binary built without `redis` feature")
        }
    }
}

fn build_memory_limiter(config: &Config) -> MemoryRateLimiter {
    let overrides = config
        .ratelimit
        .overrides
        .iter()
        .map(|(k, v)| (k.clone(), v.global_limit))
        .collect();

    MemoryRateLimiter::new(ManagerConfig {
        global_limit_default: config.ratelimit.global_limit_default,
        queue_timeout_ms: config.ratelimit.queue_timeout_ms,
        overrides,
        token_error_threshold: config.protection.consecutive_error_threshold,
        webhook_404_threshold: config.protection.consecutive_404_threshold,
    })
}

#[cfg(feature = "redis")]
fn redis_config_for(
    redis: &RedisConfig,
    config: &Config,
) -> weir_ratelimit::redis_backend::RedisConfig {
    let overrides = config
        .ratelimit
        .overrides
        .iter()
        .map(|(k, v)| (k.clone(), v.global_limit))
        .collect();

    weir_ratelimit::redis_backend::RedisConfig {
        url: redis.url.clone(),
        cluster_nodes: redis.cluster_nodes.clone(),
        key_prefix: redis.key_prefix.clone(),
        connect_timeout: Duration::from_millis(redis.connect_timeout_ms),
        command_timeout: Duration::from_millis(redis.command_timeout_ms),
        l1_cache_ttl: Duration::from_millis(redis.l1_cache_ttl_ms),
        global_limit_default: config.ratelimit.global_limit_default,
        queue_timeout: Duration::from_millis(config.ratelimit.queue_timeout_ms),
        token_error_threshold: config.protection.consecutive_error_threshold,
        webhook_404_threshold: config.protection.consecutive_404_threshold,
        overrides,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn drain_deadline_starts_at_the_signal() {
        let (tx, rx) = oneshot::channel();
        let deadline = drain_deadline(rx, Duration::from_secs(30));
        tokio::pin!(deadline);

        assert!(
            tokio::time::timeout(Duration::from_mins(1), &mut deadline)
                .await
                .is_err(),
            "must not fire before the shutdown signal"
        );

        tx.send(()).unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(29), &mut deadline)
                .await
                .is_err(),
            "must wait the full timeout after the signal"
        );
        assert!(tokio::time::timeout(Duration::from_secs(2), &mut deadline)
            .await
            .is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn drain_deadline_never_fires_without_a_sender() {
        let (tx, rx) = oneshot::channel::<()>();
        drop(tx);

        assert!(tokio::time::timeout(
            Duration::from_hours(1),
            drain_deadline(rx, Duration::from_millis(1))
        )
        .await
        .is_err());
    }
}
