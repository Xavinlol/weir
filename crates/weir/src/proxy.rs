use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use metrics::{counter, gauge, histogram};
use tracing::{debug, error, warn};
use weir_ratelimit::memory::{AcquireResult, AuthType, HealthEvent};
use weir_ratelimit::route::parse_bucket_key;

use crate::request::Auth;
use crate::response::{has_via_header, RateLimitHeaders, RateLimitScope};
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
    let auth_type = extract_auth_type(&parts.headers);
    let bucket_key = parse_bucket_key(&method_str, path);
    let is_interaction = bucket_key.is_interaction();

    let (auth_label, bot_id) = match &auth_type {
        AuthType::Bot(id) => ("bot", id.as_str()),
        AuthType::Bearer(id) => ("bearer", id.as_str()),
        AuthType::Webhook => ("webhook", ""),
    };
    let route_label = metrics_route(path);

    #[allow(clippy::cast_precision_loss)]
    gauge!("weir_active_buckets").set(state.rate_limiter.bucket_count() as f64);
    gauge!("weir_invalid_request_count")
        .set(f64::from(state.rate_limiter.invalid_requests.count()));
    gauge!("weir_cloudflare_blocked").set(
        if state.rate_limiter.cloudflare.is_blocked().is_some() {
            1.0
        } else {
            0.0
        },
    );

    match state
        .rate_limiter
        .acquire(&auth_type, &bucket_key, is_interaction)
        .await
    {
        AcquireResult::Allowed => {}
        AcquireResult::CloudflareLimited { retry_after } => {
            warn!("cloudflare rate limited");
            counter!("weir_rate_limited_total", "kind" => "cloudflare").increment(1);
            record_request(
                &method_str,
                "429",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(rate_limit_response(retry_after, false));
        }
        AcquireResult::GlobalLimited { retry_after } => {
            debug!("global rate limited");
            counter!("weir_rate_limited_total", "kind" => "global").increment(1);
            record_request(
                &method_str,
                "429",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(rate_limit_response(retry_after, true));
        }
        AcquireResult::BucketLimited { retry_after } => {
            debug!(bucket = %bucket_key, "bucket rate limited");
            counter!("weir_rate_limited_total", "kind" => "bucket").increment(1);
            record_request(
                &method_str,
                "429",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(rate_limit_response(retry_after, false));
        }
        AcquireResult::QueueTimeout => {
            debug!(bucket = %bucket_key, "queue timeout");
            counter!("weir_rate_limited_total", "kind" => "queue_timeout").increment(1);
            record_request(
                &method_str,
                "429",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(rate_limit_response(Duration::from_secs(1), false));
        }
        AcquireResult::TokenDisabled => {
            warn!("token disabled due to consecutive errors");
            counter!("weir_rate_limited_total", "kind" => "token_disabled").increment(1);
            record_request(
                &method_str,
                "403",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(error_response(StatusCode::FORBIDDEN, "token disabled"));
        }
        AcquireResult::WebhookDisabled => {
            debug!(webhook_id = %bucket_key.major_id, "webhook auto-disabled");
            counter!("weir_rate_limited_total", "kind" => "webhook_disabled").increment(1);
            record_request(
                &method_str,
                "404",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            return Err(error_response(StatusCode::NOT_FOUND, "webhook disabled"));
        }
    }

    let target_url = build_target_url(&uri);

    let body_bytes = axum::body::to_bytes(body, MAX_BODY_SIZE)
        .await
        .map_err(|e| {
            warn!(error = %e, "failed to read request body");
            record_request(
                &method_str,
                "400",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
            error_response(
                StatusCode::BAD_REQUEST,
                "request body too large or unreadable",
            )
        })?;

    let mut outgoing = state
        .http_client
        .request(method, &target_url)
        .body(reqwest::Body::from(body_bytes))
        .build()
        .map_err(|e| {
            warn!(error = %e, "failed to build outgoing request");
            record_request(
                &method_str,
                "500",
                auth_label,
                bot_id,
                &route_label,
                request_start,
            );
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
        record_request(
            &method_str,
            "502",
            auth_label,
            bot_id,
            &route_label,
            request_start,
        );
        error_response(StatusCode::BAD_GATEWAY, "discord request failed")
    })?;
    histogram!("weir_discord_latency_seconds", "route" => route_label.clone())
        .record(discord_start.elapsed().as_secs_f64());

    let status = response.status();
    let headers = response.headers().clone();
    let body_bytes = response.bytes().await.map_err(|e| {
        warn!(error = %e, "failed to read discord response body");
        record_request(
            &method_str,
            "502",
            auth_label,
            bot_id,
            &route_label,
            request_start,
        );
        error_response(StatusCode::BAD_GATEWAY, "failed to read response body")
    })?;

    // Forensic dump on 401 so we can debug auth failures
    if status == StatusCode::UNAUTHORIZED {
        let resp_body = String::from_utf8_lossy(&body_bytes);
        let safe_url = redact_url_tokens(&target_url);
        let req_headers: Vec<String> = parts
            .headers
            .iter()
            .map(|(name, value)| {
                let v = if name.as_str() == "authorization" {
                    let s = value.to_str().unwrap_or("<binary>");
                    if s.len() > 20 {
                        format!("{}...{}", &s[..10], &s[s.len() - 5..])
                    } else {
                        "<short>".into()
                    }
                } else {
                    value.to_str().unwrap_or("<binary>").to_owned()
                };
                format!("{name}: {v}")
            })
            .collect();
        warn!(
            method = %method_str,
            url = %safe_url,
            auth_type = %auth_label,
            response = %resp_body,
            headers = ?req_headers,
            "401 UNAUTHORIZED from Discord"
        );
    }

    let rl_headers = RateLimitHeaders::from_headers(&headers);
    let has_via = has_via_header(&headers);

    match state
        .rate_limiter
        .report_response(&auth_type, &bucket_key, status.as_u16(), has_via)
    {
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

    record_request(
        &method_str,
        &status_str,
        auth_label,
        bot_id,
        &route_label,
        request_start,
    );

    if status.is_server_error()
        || (status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS)
    {
        counter!("weir_discord_errors_total", "status" => status_str).increment(1);
    }

    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder = builder.header("x-sent-by-proxy", "weir");

    builder.body(Body::from(body_bytes)).map_err(|e| {
        warn!(error = %e, "failed to build proxy response");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build response",
        )
    })
}

fn record_request(
    method: &str,
    status: &str,
    auth_label: &str,
    bot_id: &str,
    route: &str,
    start: Instant,
) {
    counter!("weir_requests_total",
        "method" => method.to_owned(),
        "status" => status.to_owned(),
        "auth_type" => auth_label.to_owned(),
        "bot_id" => bot_id.to_owned(),
        "route" => route.to_owned()
    )
    .increment(1);
    histogram!("weir_request_duration_seconds",
        "method" => method.to_owned(),
        "route" => route.to_owned()
    )
    .record(start.elapsed().as_secs_f64());
}

fn extract_auth_type(headers: &axum::http::HeaderMap) -> AuthType {
    if let Some(auth_header) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let auth = Auth::from_header(auth_header);
        match auth {
            Auth::Bot { bot_id } if !bot_id.is_empty() => AuthType::Bot(bot_id),
            Auth::Bearer { bot_id } if !bot_id.is_empty() => AuthType::Bearer(bot_id),
            Auth::Bot { .. } | Auth::Bearer { .. } => {
                warn!("auth header present but bot_id extraction failed, treating as webhook");
                AuthType::Webhook
            }
            Auth::None => AuthType::Webhook,
        }
    } else {
        AuthType::Webhook
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
        warn!(
            retry_after_ms = retry_after.as_millis(),
            "cloudflare 429 detected (no via header)"
        );
        counter!("weir_discord_429_total", "scope" => "cloudflare").increment(1);
    } else if is_global {
        warn!(
            retry_after_ms = retry_after.as_millis(),
            "global 429 from discord"
        );
        counter!("weir_discord_429_total", "scope" => "global").increment(1);
    } else {
        debug!(bucket = %key, retry_after_ms = retry_after.as_millis(), "per-route 429 from discord");
        counter!("weir_discord_429_total", "scope" => "per_route").increment(1);
    }

    state
        .rate_limiter
        .handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after);

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
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

/// Redact long path segments (tokens) from URLs before logging.
fn redact_url_tokens(url: &str) -> String {
    // Split at the path portion after the host
    let (prefix, path_and_query) = if let Some(pos) = url.find("discord.com") {
        let host_end = pos + "discord.com".len();
        (&url[..host_end], &url[host_end..])
    } else {
        return url.to_owned();
    };

    let (path, query) = match path_and_query.find('?') {
        Some(pos) => (&path_and_query[..pos], Some(&path_and_query[pos..])),
        None => (path_and_query, None),
    };

    let redacted_path: String = path
        .split('/')
        .map(|seg| if seg.len() >= 60 { "<redacted>" } else { seg })
        .collect::<Vec<_>>()
        .join("/");

    match query {
        Some(q) => format!("{prefix}{redacted_path}{q}"),
        None => format!("{prefix}{redacted_path}"),
    }
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

/// Normalize a Discord API path into a bounded-cardinality metrics label.
///
/// Replaces snowflake IDs with `{id}`, long tokens (webhook/interaction) with
/// `{token}`, and other variable segments (emoji, etc.) with `{val}`.
/// Keeps keywords like `messages`, `@me`, `@original` intact.
fn metrics_route(path: &str) -> String {
    let path = path.trim_start_matches('/');
    // Strip query string
    let path = path.split('?').next().unwrap_or(path);
    // Strip /api/v{N}/ prefix
    let path = if let Some(rest) = path.strip_prefix("api/") {
        if rest.starts_with('v') && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit) {
            rest.split_once('/').map_or(rest, |(_, after)| after)
        } else {
            rest
        }
    } else {
        path
    };

    let mut result = String::with_capacity(path.len());

    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        result.push('/');

        if segment.starts_with('@') || segment.starts_with("%40") {
            result.push('@');
            let name = segment
                .strip_prefix('@')
                .or_else(|| segment.strip_prefix("%40"))
                .unwrap_or(segment);
            result.push_str(name);
        } else if !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()) {
            result.push_str("{id}");
        } else if segment.len() >= 60 {
            result.push_str("{token}");
        } else if is_api_keyword(segment) {
            result.push_str(segment);
        } else {
            result.push_str("{val}");
        }
    }

    if result.is_empty() {
        result.push('/');
    }

    result
}

/// Returns true for segments that are fixed Discord API path keywords.
/// Matches lowercase alpha with optional hyphens (e.g., `messages`, `auto-moderation`).
#[inline]
fn is_api_keyword(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|&b| b.is_ascii_lowercase() || b == b'-')
}

fn rate_limit_response(retry_after: Duration, is_global: bool) -> Response<Body> {
    let retry_secs = retry_after.as_secs_f64();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let retry_header = format!("{}", retry_secs.ceil() as u64);
    let body = format!(
        r#"{{"message":"You are being rate limited.","retry_after":{retry_secs:.3},"global":{is_global},"proxy":"weir"}}"#
    );

    Response::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", HeaderValue::from_static("application/json"))
        .header(
            "retry-after",
            HeaderValue::from_str(&retry_header).unwrap_or(HeaderValue::from_static("1")),
        )
        .header("x-sent-by-proxy", HeaderValue::from_static("weir"))
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_route_channels_messages() {
        assert_eq!(
            metrics_route("/api/v10/channels/123456789012345678/messages/987654321098765432"),
            "/channels/{id}/messages/{id}"
        );
    }

    #[test]
    fn metrics_route_channels_messages_no_api_prefix() {
        assert_eq!(
            metrics_route("/channels/123456789012345678/messages"),
            "/channels/{id}/messages"
        );
    }

    #[test]
    fn metrics_route_webhooks_with_token() {
        let token = "abcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let path = format!("/api/v10/webhooks/123456789012345678/{token}/messages/@original");
        assert_eq!(
            metrics_route(&path),
            "/webhooks/{id}/{token}/messages/@original"
        );
    }

    #[test]
    fn metrics_route_interactions_callback() {
        let token = "aW50ZXJhY3Rpb246MTIzNDU2Nzg5MDEyMzQ1Njc4OjE2OTk5OTk5OTk6dGVzdA";
        let path = format!("/api/v10/interactions/123456789012345678/{token}/callback");
        assert_eq!(metrics_route(&path), "/interactions/{id}/{token}/callback");
    }

    #[test]
    fn metrics_route_users_me() {
        assert_eq!(metrics_route("/api/v10/users/@me"), "/users/@me");
    }

    #[test]
    fn metrics_route_gateway_bot() {
        assert_eq!(metrics_route("/api/v10/gateway/bot"), "/gateway/bot");
    }

    #[test]
    fn metrics_route_applications_commands() {
        assert_eq!(
            metrics_route("/api/v10/applications/123456789012345678/commands"),
            "/applications/{id}/commands"
        );
    }

    #[test]
    fn metrics_route_guilds_channels() {
        assert_eq!(
            metrics_route("/api/v10/guilds/123456789012345678/channels"),
            "/guilds/{id}/channels"
        );
    }

    #[test]
    fn metrics_route_webhook_execute() {
        let token = "abcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let path = format!("/api/v10/webhooks/123456789012345678/{token}");
        assert_eq!(metrics_route(&path), "/webhooks/{id}/{token}");
    }

    #[test]
    fn metrics_route_reaction_emoji() {
        assert_eq!(
            metrics_route("/api/v10/channels/123456789012345678/messages/987654321098765432/reactions/%F0%9F%91%8D/@me"),
            "/channels/{id}/messages/{id}/reactions/{val}/@me"
        );
    }

    #[test]
    fn metrics_route_unversioned_api() {
        assert_eq!(
            metrics_route("/api/channels/123456789012345678/messages"),
            "/channels/{id}/messages"
        );
    }

    #[test]
    fn metrics_route_auto_moderation() {
        assert_eq!(
            metrics_route("/api/v10/guilds/123456789012345678/auto-moderation/rules"),
            "/guilds/{id}/auto-moderation/rules"
        );
    }

    #[test]
    fn metrics_route_empty_path() {
        assert_eq!(metrics_route("/"), "/");
    }

    #[test]
    fn metrics_route_query_string_stripped() {
        assert_eq!(
            metrics_route("/api/v10/channels/123456789012345678/messages?limit=50"),
            "/channels/{id}/messages"
        );
    }

    #[test]
    fn metrics_route_api_non_version_v_prefix() {
        assert_eq!(metrics_route("/api/voice/regions"), "/voice/regions");
    }

    #[test]
    fn metrics_route_url_encoded_at_original() {
        let token = "abcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890";
        let path = format!("/api/v10/webhooks/123456789012345678/{token}/messages/%40original");
        assert_eq!(
            metrics_route(&path),
            "/webhooks/{id}/{token}/messages/@original"
        );
    }
}
