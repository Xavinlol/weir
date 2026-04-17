use std::net::SocketAddr;

use metrics::{describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::PrometheusBuilder;

pub fn init(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let latency_buckets = [
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];

    PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets(&latency_buckets)?
        .install()?;

    describe_counter!("weir_requests_total", "Total requests proxied through Weir");
    describe_counter!(
        "weir_rate_limited_total",
        "Requests rate-limited by the proxy before reaching Discord"
    );
    describe_counter!(
        "weir_discord_429_total",
        "429 responses returned by Discord"
    );
    describe_counter!(
        "weir_invalid_requests_total",
        "Requests counted toward the 10k/10min invalid request limit"
    );
    describe_counter!(
        "weir_protection_events_total",
        "Protection events (token disabled, webhook disabled, CF ban)"
    );
    describe_counter!(
        "weir_discord_errors_total",
        "Non-2xx responses from Discord"
    );

    describe_histogram!(
        "weir_request_duration_seconds",
        "Full proxy round-trip latency"
    );
    describe_histogram!(
        "weir_discord_latency_seconds",
        "Latency of the Discord HTTP request only"
    );

    describe_gauge!(
        "weir_active_buckets",
        "Current number of rate limit buckets in memory"
    );
    describe_gauge!(
        "weir_invalid_request_count",
        "Current invalid request count in the 10-minute window"
    );
    describe_gauge!(
        "weir_cloudflare_blocked",
        "Whether the proxy is currently Cloudflare-blocked (0 or 1)"
    );

    Ok(())
}
