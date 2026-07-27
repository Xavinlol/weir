use std::time::Duration;

use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis;
use weir_ratelimit::memory::{AcquireResult, AuthType};
use weir_ratelimit::redis_backend::{RedisConfig, RedisRateLimiter};
use weir_ratelimit::route::{parse_bucket_key, BucketKey};

async fn start_redis() -> (testcontainers::ContainerAsync<Redis>, String) {
    let container = Redis::default().start().await.expect("redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");
    (container, url)
}

fn config_for(url: String) -> RedisConfig {
    RedisConfig {
        url,
        key_prefix: "weir:test:".to_owned(),
        global_limit_default: 50,
        queue_timeout: Duration::from_millis(100),
        l1_cache_ttl: Duration::from_millis(10),
        ..RedisConfig::default()
    }
}

fn channels_key(major: &str) -> BucketKey {
    parse_bucket_key("GET", &format!("/api/v10/channels/{major}/messages"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_instances_share_bucket() {
    let (_container, url) = start_redis().await;

    let cfg = config_for(url);
    let pod_a = RedisRateLimiter::new(cfg.clone()).await.expect("pod a");
    let pod_b = RedisRateLimiter::new(cfg).await.expect("pod b");

    let auth = AuthType::Bot("123".to_owned());
    let key = channels_key("456");

    let allow_a = pod_a.acquire(&auth, &key, false).await;
    assert!(matches!(allow_a, AcquireResult::Allowed));

    pod_a
        .update_from_response(&auth, &key, Some("abc"), Some(1), Some(1), Some(5.0))
        .await;
    pod_a
        .update_from_response(&auth, &key, Some("abc"), Some(0), Some(1), Some(5.0))
        .await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    let denied = pod_b.acquire(&auth, &key, false).await;
    assert!(
        matches!(denied, AcquireResult::BucketLimited { .. }),
        "pod B should see drained bucket, got {denied:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cf_ban_propagates() {
    let (_container, url) = start_redis().await;

    let cfg = config_for(url);
    let pod_a = RedisRateLimiter::new(cfg.clone()).await.expect("pod a");
    let pod_b = RedisRateLimiter::new(cfg).await.expect("pod b");

    let auth = AuthType::Bot("999".to_owned());
    let key = channels_key("1");

    let _ = pod_a.report_response(&auth, &key, 403, false).await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(pod_b.is_cloudflare_blocked().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_applies_on_redis_hot_path() {
    let (_container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.global_limit_default = 1;
    cfg.overrides.insert("vip".to_owned(), 3);

    let pod = RedisRateLimiter::new(cfg).await.expect("pod");

    let vip = AuthType::Bot("vip".to_owned());
    let normal = AuthType::Bot("normal".to_owned());
    let key = channels_key("1");

    for _ in 0..3 {
        let r = pod.acquire(&vip, &key, false).await;
        assert!(matches!(r, AcquireResult::Allowed), "vip slot {r:?}");
    }
    assert!(matches!(
        pod.acquire(&vip, &key, false).await,
        AcquireResult::GlobalLimited { .. }
    ));

    assert!(matches!(
        pod.acquire(&normal, &key, false).await,
        AcquireResult::Allowed
    ));
    assert!(matches!(
        pod.acquire(&normal, &key, false).await,
        AcquireResult::GlobalLimited { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_limit_enforced_on_learned_route() {
    let (_container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.global_limit_default = 2;
    let pod = RedisRateLimiter::new(cfg).await.expect("pod");

    let auth = AuthType::Bot("g1".to_owned());
    let key = channels_key("1");

    pod.update_from_response(&auth, &key, Some("bh"), Some(100), Some(100), Some(60.0))
        .await;

    for slot in 0..2 {
        let r = pod.acquire(&auth, &key, false).await;
        assert!(matches!(r, AcquireResult::Allowed), "slot {slot}: {r:?}");
    }

    let denied = pod.acquire(&auth, &key, false).await;
    assert!(
        matches!(denied, AcquireResult::GlobalLimited { .. }),
        "global limit must apply to a learned route, got {denied:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_ban_denies_learned_route() {
    let (_container, url) = start_redis().await;

    let pod = RedisRateLimiter::new(config_for(url)).await.expect("pod");

    let auth = AuthType::Bot("g2".to_owned());
    let key = channels_key("1");

    pod.update_from_response(&auth, &key, Some("bh"), Some(100), Some(100), Some(60.0))
        .await;
    pod.handle_rate_limit(&auth, &key, true, false, Duration::from_secs(5))
        .await;

    let denied = pod.acquire(&auth, &key, false).await;
    assert!(
        matches!(denied, AcquireResult::GlobalLimited { .. }),
        "global ban must outrank bucket headroom, got {denied:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_learned_without_numeric_headers() {
    let (_container, url) = start_redis().await;

    let pod = RedisRateLimiter::new(config_for(url)).await.expect("pod");

    let auth = AuthType::Bot("g3".to_owned());
    let key = channels_key("1");

    pod.update_from_response(&auth, &key, Some("bh"), None, None, None)
        .await;
    pod.handle_rate_limit(&auth, &key, false, false, Duration::from_secs(5))
        .await;

    let denied = pod.acquire(&auth, &key, false).await;
    assert!(
        matches!(denied, AcquireResult::BucketLimited { .. }),
        "route should have been learned from the hash alone, got {denied:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactions_bypass_global_limit() {
    let (_container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.global_limit_default = 1;
    let pod = RedisRateLimiter::new(cfg).await.expect("pod");

    let auth = AuthType::Bot("g4".to_owned());
    let key = channels_key("1");

    pod.update_from_response(&auth, &key, Some("bh"), Some(100), Some(100), Some(60.0))
        .await;

    assert!(matches!(
        pod.acquire(&auth, &key, false).await,
        AcquireResult::Allowed
    ));
    for _ in 0..3 {
        let r = pod.acquire(&auth, &key, true).await;
        assert!(matches!(r, AcquireResult::Allowed), "interaction {r:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_disable_propagates_across_pods() {
    let (_container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.token_error_threshold = 3;
    let pod_a = RedisRateLimiter::new(cfg.clone()).await.expect("pod a");
    let pod_b = RedisRateLimiter::new(cfg).await.expect("pod b");

    let auth = AuthType::Bot("dead".to_owned());
    let key = channels_key("1");

    for _ in 0..3 {
        pod_a.report_response(&auth, &key, 401, true).await;
    }

    tokio::time::sleep(Duration::from_millis(20)).await;

    let denied = pod_b.acquire(&auth, &key, false).await;
    assert!(
        matches!(denied, AcquireResult::TokenDisabled),
        "pod B should see the disabled token, got {denied:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_success_prevents_cross_pod_disable() {
    let (_container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.token_error_threshold = 3;
    cfg.l1_cache_ttl = Duration::from_millis(500);
    let pod_a = RedisRateLimiter::new(cfg.clone()).await.expect("pod a");
    let pod_b = RedisRateLimiter::new(cfg).await.expect("pod b");

    let auth = AuthType::Bot("mixed".to_owned());
    let key = channels_key("1");

    // Warms pod B's health cache before any error exists.
    pod_b.acquire(&auth, &key, false).await;

    for _ in 0..3 {
        pod_a.report_response(&auth, &key, 401, true).await;
        pod_b.report_response(&auth, &key, 200, true).await;
    }

    let after = pod_a.acquire(&auth, &key, false).await;
    assert!(
        matches!(after, AcquireResult::Allowed),
        "interleaved successes should keep the token enabled, got {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_count_shared_across_pods() {
    let (_container, url) = start_redis().await;

    let cfg = config_for(url);
    let pod_a = RedisRateLimiter::new(cfg.clone()).await.expect("pod a");
    let pod_b = RedisRateLimiter::new(cfg).await.expect("pod b");

    for _ in 0..5 {
        pod_a.track_invalid().await;
    }
    for _ in 0..3 {
        pod_b.track_invalid().await;
    }

    tokio::time::sleep(Duration::from_millis(20)).await;

    let count = pod_a.invalid_count().await;
    assert_eq!(count, 8, "shared invalid count, got {count}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outage_falls_back() {
    let (container, url) = start_redis().await;

    let mut cfg = config_for(url);
    cfg.global_limit_default = 1;
    let pod = RedisRateLimiter::new(cfg).await.expect("pod");

    let auth = AuthType::Bot("555".to_owned());
    let key = channels_key("1");

    let r = pod.acquire(&auth, &key, false).await;
    assert!(matches!(r, AcquireResult::Allowed));

    container.stop().await.expect("stop redis");

    // Only the in-process fallback can return GlobalLimited here.
    let mut outcomes = Vec::new();
    for _ in 0..4 {
        outcomes.push(pod.acquire(&auth, &key, false).await);
    }

    assert!(
        outcomes.iter().any(|r| matches!(r, AcquireResult::Allowed)),
        "fallback should keep serving, got {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|r| matches!(r, AcquireResult::GlobalLimited { .. })),
        "fallback should enforce the global limit, got {outcomes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cf_ban_survives_redis_outage() {
    let (container, url) = start_redis().await;

    let pod = RedisRateLimiter::new(config_for(url)).await.expect("pod");

    let auth = AuthType::Bot("777".to_owned());
    let key = channels_key("1");

    let _ = pod.report_response(&auth, &key, 403, false).await;
    assert!(pod.is_cloudflare_blocked().await);

    container.stop().await.expect("stop redis");
    tokio::time::sleep(Duration::from_millis(50)).await;

    for probe in 0..4 {
        assert!(
            pod.is_cloudflare_blocked().await,
            "ban must hold across the outage (probe {probe})"
        );
    }
}
