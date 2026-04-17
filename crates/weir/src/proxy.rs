use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use tracing::{debug, warn};

use crate::server::AppState;

const DISCORD_BASE: &str = "https://discord.com";
const MAX_BODY_SIZE: usize = 25 * 1024 * 1024; // 25 MB

pub async fn handle(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response<Body>, Response<Body>> {
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let uri = parts.uri;

    debug!(%method, path = %uri.path(), "proxying request");

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
