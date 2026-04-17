use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use metrics::gauge;
use serde::Serialize;

use crate::server::AppState;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

pub async fn live() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(HealthResponse {
            status: "healthy",
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let cloudflare_blocked = state.rate_limiter.cloudflare.is_blocked().is_some();
    let invalid_count = state.rate_limiter.invalid_requests.count();
    let degraded = cloudflare_blocked || invalid_count >= 8000;

    #[allow(clippy::cast_precision_loss)]
    gauge!("weir_active_buckets").set(state.rate_limiter.bucket_count() as f64);
    gauge!("weir_invalid_request_count").set(f64::from(invalid_count));
    gauge!("weir_cloudflare_blocked").set(if cloudflare_blocked { 1.0 } else { 0.0 });

    let status = if degraded { "degraded" } else { "healthy" };
    let http_status = if degraded {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        http_status,
        Json(HealthResponse {
            status,
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}
