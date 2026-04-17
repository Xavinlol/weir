use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use tracing::{debug, error, warn};
use weir_ratelimit::memory::{AcquireResult, AuthType};
use weir_ratelimit::route::parse_bucket_key;

use crate::request::Auth;
use crate::response::{RateLimitHeaders, RateLimitScope, has_via_header};
use crate::server::AppState;

const DISCORD_BASE: &str = "https://discord.com";
const MAX_BODY_SIZE: usize = 25 * 1024 * 1024; // 25 MB

/// Discord 429 response body with `retry_after` field.
#[derive(serde::Deserialize)]
struct RateLimitBody {
    retry_after: Option<f64>,
}

#[allow(clippy::too_many_lines)]
pub async fn handle(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, Response<Body>> {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;
    let path = uri.path();
    let method_str = method.as_str();

    debug!(%method, %path, "proxying request");

    let auth_type = extract_auth_type(&parts.headers, path);
    let bucket_key = parse_bucket_key(method_str, path);
    let is_interaction = bucket_key.is_interaction();

    match state.rate_limiter.acquire(&auth_type, &bucket_key, is_interaction).await {
        AcquireResult::Allowed => {}
        AcquireResult::CloudflareLimited { retry_after } => {
            warn!("cloudflare rate limited");
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::GlobalLimited { retry_after } => {
            debug!("global rate limited");
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::BucketLimited { retry_after } => {
            debug!(bucket = %bucket_key, "bucket rate limited");
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::QueueTimeout => {
            debug!(bucket = %bucket_key, "queue timeout");
            return Err(rate_limit_response(Duration::from_secs(1)));
        }
    }

    let target_url = build_target_url(&uri);

    let body_bytes = axum::body::to_bytes(body, MAX_BODY_SIZE)
        .await
        .map_err(|e| {
            warn!(error = %e, "failed to read request body");
            error_response(StatusCode::BAD_REQUEST, "request body too large or unreadable")
        })?;

    let mut outgoing = state
        .http_client
        .request(method, &target_url)
        .body(reqwest::Body::from(body_bytes))
        .build()
        .map_err(|e| {
            warn!(error = %e, "failed to build outgoing request");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to build request")
        })?;

    for (name, value) in &parts.headers {
        if !is_hop_by_hop(name.as_str()) {
            outgoing.headers_mut().insert(name.clone(), value.clone());
        }
    }

    let response = state.http_client.execute(outgoing).await.map_err(|e| {
        warn!(error = %e, "discord request failed");
        error_response(StatusCode::BAD_GATEWAY, "discord request failed")
    })?;

    let status = response.status();
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        warn!(error = %e, "failed to read discord response body");
        error_response(StatusCode::BAD_GATEWAY, "failed to read response body")
    })?;

    let rl_headers = RateLimitHeaders::from_headers(&headers);

    let is_invalid = matches!(status.as_u16(), 401 | 403)
        || (status == StatusCode::TOO_MANY_REQUESTS
            && rl_headers.scope != Some(RateLimitScope::Shared)
            && has_via_header(&headers));
    if is_invalid {
        let count = state.rate_limiter.invalid_requests.track();
        if count >= 9500 {
            error!(count, "approaching invalid request limit (10000/10min)");
        } else if count >= 8000 {
            warn!(count, "high invalid request count (10000/10min)");
        }
    }

    if status == StatusCode::TOO_MANY_REQUESTS {
        handle_429(
            &state,
            &auth_type,
            &bucket_key,
            &rl_headers,
            &headers,
            &body_bytes,
        );
    } else {
        state.rate_limiter.update_from_response(
            &auth_type,
            &bucket_key,
            rl_headers.bucket.as_deref(),
            rl_headers.remaining,
            rl_headers.limit,
            rl_headers.reset_after,
        );
    }

    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder = builder.header("x-sent-by-proxy", "weir");

    builder.body(Body::from(body_bytes)).map_err(|e| {
        warn!(error = %e, "failed to build proxy response");
        error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to build response")
    })
}

fn extract_auth_type(headers: &axum::http::HeaderMap, _path: &str) -> AuthType {
    if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let auth = Auth::from_header(auth_header);
        match auth {
            Auth::Bot { bot_id } if !bot_id.is_empty() => AuthType::Bot(bot_id),
            Auth::Bearer { bot_id } if !bot_id.is_empty() => AuthType::Bearer(bot_id),
            _ => AuthType::Webhook,
        }
    } else {
        AuthType::Webhook
    }
}

/// Check if path matches /webhooks/{id}/{token} or /api/v{N}/webhooks/{id}/{token}.
#[allow(dead_code)]
fn is_webhook_path(path: &str) -> bool {
    let path = path.trim_start_matches('/');
    let path = if let Some(rest) = path.strip_prefix("api/") {
        if let Some(pos) = rest.find('/') {
            &rest[pos + 1..]
        } else {
            return false;
        }
    } else {
        path
    };

    if let Some(rest) = path.strip_prefix("webhooks/") {
        rest.contains('/')
    } else {
        false
    }
}

fn handle_429(
    state: &AppState,
    auth: &AuthType,
    key: &weir_ratelimit::route::BucketKey,
    rl_headers: &RateLimitHeaders,
    raw_headers: &axum::http::HeaderMap,
    body: &[u8],
) {
    let is_global = rl_headers.is_global;
    let is_cloudflare = !has_via_header(raw_headers);

    let header_retry = rl_headers.reset_after.unwrap_or(1.0);
    let body_retry = serde_json::from_slice::<RateLimitBody>(body)
        .ok()
        .and_then(|b| b.retry_after)
        .unwrap_or(0.0);
    let retry_after_secs = header_retry.max(body_retry);

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let retry_after = Duration::from_millis((retry_after_secs * 1000.0) as u64);

    if is_cloudflare {
        warn!(retry_after_ms = retry_after.as_millis(), "cloudflare 429 detected (no via header)");
    } else if is_global {
        warn!(retry_after_ms = retry_after.as_millis(), "global 429 from discord");
    } else {
        debug!(bucket = %key, retry_after_ms = retry_after.as_millis(), "per-route 429 from discord");
    }

    state.rate_limiter.handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after);

    state.rate_limiter.update_from_response(
        auth,
        key,
        rl_headers.bucket.as_deref(),
        Some(0),
        rl_headers.limit,
        rl_headers.reset_after,
    );
}

#[inline]
fn build_target_url(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    match uri.query() {
        Some(q) => format!("{DISCORD_BASE}{path}?{q}"),
        None => format!("{DISCORD_BASE}{path}"),
    }
}

#[inline]
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host" | "connection" | "transfer-encoding" | "content-length"
    )
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = format!(r#"{{"error":"{message}","proxy":"weir"}}"#);

    Response::builder()
        .status(status)
        .header("content-type", HeaderValue::from_static("application/json"))
        .header("x-sent-by-proxy", HeaderValue::from_static("weir"))
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn rate_limit_response(retry_after: Duration) -> Response<Body> {
    let retry_secs = retry_after.as_secs_f64();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let retry_header = format!("{}", retry_secs.ceil() as u64);
    let body = format!(
        r#"{{"message":"You are being rate limited.","retry_after":{retry_secs:.3},"global":false,"proxy":"weir"}}"#
    );

    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", HeaderValue::from_static("application/json"))
        .header("retry-after", HeaderValue::from_str(&retry_header).unwrap_or(HeaderValue::from_static("1")))
        .header("x-sent-by-proxy", HeaderValue::from_static("weir"))
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
