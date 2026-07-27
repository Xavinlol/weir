use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::server::AppState;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 80% of Discord's 10k invalid requests per 10 minutes.
const INVALID_REQUEST_THRESHOLD: u32 = 8000;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
pub struct UpstreamResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub cloudflare_blocked: bool,
    pub invalid_requests: u32,
}

fn healthy() -> (StatusCode, Json<HealthResponse>) {
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "healthy",
            version: VERSION,
        }),
    )
}

pub async fn live() -> impl IntoResponse {
    healthy()
}

/// Ignores Cloudflare bans and the invalid budget on purpose: that state is
/// shared on the Redis backend, so it belongs on `/health/upstream`.
pub async fn ready() -> impl IntoResponse {
    healthy()
}

/// How Discord sees the fleet. For alerts, not probes.
pub async fn upstream(State(state): State<AppState>) -> impl IntoResponse {
    let cloudflare_blocked = state.rate_limiter.is_cloudflare_blocked().await;
    let invalid_requests = state.rate_limiter.invalid_count().await;
    let degraded = cloudflare_blocked || invalid_requests >= INVALID_REQUEST_THRESHOLD;

    let (code, status) = if degraded {
        (StatusCode::SERVICE_UNAVAILABLE, "degraded")
    } else {
        (StatusCode::OK, "healthy")
    };

    (
        code,
        Json(UpstreamResponse {
            status,
            version: VERSION,
            cloudflare_blocked,
            invalid_requests,
        }),
    )
}
