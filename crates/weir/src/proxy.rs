use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use metrics::{counter, histogram};
use tracing::{debug, error, warn};
use weir_ratelimit::memory::{AcquireResult, AuthType, HealthEvent};
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
    let method_str = method.as_str().to_owned();

    debug!(%method, %path, "proxying request");

    let request_start = Instant::now();
    let auth_type = extract_auth_type(&parts.headers, path);
    let bucket_key = parse_bucket_key(&method_str, path);
    let is_interaction = bucket_key.is_interaction();

    match state.rate_limiter.acquire(&auth_type, &bucket_key, is_interaction).await {
        AcquireResult::Allowed => {}
        AcquireResult::CloudflareLimited { retry_after } => {
            warn!("cloudflare rate limited");
            counter!("weir_rate_limited_total", "kind" => "cloudflare").increment(1);
            record_request(&method_str, "429", &auth_type, request_start);
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::GlobalLimited { retry_after } => {
            debug!("global rate limited");
            counter!("weir_rate_limited_total", "kind" => "global").increment(1);
            record_request(&method_str, "429", &auth_type, request_start);
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::BucketLimited { retry_after } => {
            debug!(bucket = %bucket_key, "bucket rate limited");
            counter!("weir_rate_limited_total", "kind" => "bucket").increment(1);
            record_request(&method_str, "429", &auth_type, request_start);
            return Err(rate_limit_response(retry_after));
        }
        AcquireResult::QueueTimeout => {
            debug!(bucket = %bucket_key, "queue timeout");
            counter!("weir_rate_limited_total", "kind" => "queue_timeout").increment(1);
            record_request(&method_str, "429", &auth_type, request_start);
            return Err(rate_limit_response(Duration::from_secs(1)));
        }
        AcquireResult::TokenDisabled => {
            warn!("token disabled due to consecutive errors");
            counter!("weir_rate_limited_total", "kind" => "token_disabled").increment(1);
            record_request(&method_str, "403", &auth_type, request_start);
            return Err(error_response(StatusCode::FORBIDDEN, "token disabled"));
        }
        AcquireResult::WebhookDisabled => {
            debug!(webhook_id = %bucket_key.major_id, "webhook auto-disabled");
            counter!("weir_rate_limited_total", "kind" => "webhook_disabled").increment(1);
            record_request(&method_str, "404", &auth_type, request_start);
            return Err(error_response(StatusCode::NOT_FOUND, "webhook disabled"));
        }
    }

    let target_url = build_target_url(&uri);

    let body_bytes = axum::body::to_bytes(body, MAX_BODY_SIZE)
        .await
        .map_err(|e| {
            warn!(error = %e, "failed to read request body");
            record_request(&method_str, "400", &auth_type, request_start);
            error_response(StatusCode::BAD_REQUEST, "request body too large or unreadable")
        })?;

    let mut outgoing = state
        .http_client
        .request(method, &target_url)
        .body(reqwest::Body::from(body_bytes))
        .build()
        .map_err(|e| {
            warn!(error = %e, "failed to build outgoing request");
            record_request(&method_str, "500", &auth_type, request_start);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "failed to build request")
        })?;

    for (name, value) in &parts.headers {
        if !is_hop_by_hop(name.as_str()) {
            outgoing.headers_mut().insert(name.clone(), value.clone());
        }
    }

    let discord_start = Instant::now();
    let response = state.http_client.execute(outgoing).await.map_err(|e| {
        warn!(error = %e, "discord request failed");
        record_request(&method_str, "502", &auth_type, request_start);
        error_response(StatusCode::BAD_GATEWAY, "discord request failed")
    })?;
    histogram!("weir_discord_latency_seconds").record(discord_start.elapsed().as_secs_f64());

    let status = response.status();
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        warn!(error = %e, "failed to read discord response body");
        record_request(&method_str, "502", &auth_type, request_start);
        error_response(StatusCode::BAD_GATEWAY, "failed to read response body")
    })?;

    let rl_headers = RateLimitHeaders::from_headers(&headers);
    let has_via = has_via_header(&headers);

    match state.rate_limiter.report_response(&auth_type, &bucket_key, status.as_u16(), has_via) {
        HealthEvent::TokenDisabled => {
            warn!("token auto-disabled due to consecutive errors");
            counter!("weir_protection_events_total", "event" => "token_disabled").increment(1);
        }
        HealthEvent::WebhookDisabled => {
            warn!(webhook_id = %bucket_key.major_id, "webhook auto-disabled");
            counter!("weir_protection_events_total", "event" => "webhook_disabled").increment(1);
        }
        HealthEvent::CloudflareBanned => {
            warn!("cloudflare ban detected (403 without via header)");
            counter!("weir_protection_events_total", "event" => "cloudflare_banned").increment(1);
        }
        HealthEvent::None => {}
    }

    let is_invalid = has_via
        && (matches!(status.as_u16(), 401 | 403)
            || (status == StatusCode::TOO_MANY_REQUESTS
                && rl_headers.scope != Some(RateLimitScope::Shared)));
    if is_invalid {
        counter!("weir_invalid_requests_total").increment(1);
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
            has_via,
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

    let status_str = status.as_str().to_owned();

    record_request(&method_str, &status_str, &auth_type, request_start);

    if status.is_server_error() || (status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS) {
        counter!("weir_discord_errors_total", "status" => status_str).increment(1);
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

fn record_request(method: &str, status: &str, auth_type: &AuthType, start: Instant) {
    let auth_label = match auth_type {
        AuthType::Bot(_) => "bot",
        AuthType::Bearer(_) => "bearer",
        AuthType::Webhook => "webhook",
    };
    counter!("weir_requests_total", "method" => method.to_owned(), "status" => status.to_owned(), "auth_type" => auth_label).increment(1);
    histogram!("weir_request_duration_seconds", "method" => method.to_owned(), "status" => status.to_owned()).record(start.elapsed().as_secs_f64());
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
    has_via: bool,
    body: &[u8],
) {
    let is_global = rl_headers.is_global;
    let is_cloudflare = !has_via;

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
        counter!("weir_discord_429_total", "scope" => "cloudflare").increment(1);
    } else if is_global {
        warn!(retry_after_ms = retry_after.as_millis(), "global 429 from discord");
        counter!("weir_discord_429_total", "scope" => "global").increment(1);
    } else {
        debug!(bucket = %key, retry_after_ms = retry_after.as_millis(), "per-route 429 from discord");
        counter!("weir_discord_429_total", "scope" => "per_route").increment(1);
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
