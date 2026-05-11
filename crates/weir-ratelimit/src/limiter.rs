use std::time::Duration;

use crate::memory::{AcquireResult, AuthType, HealthEvent, MemoryRateLimiter};
#[cfg(feature = "redis")]
use crate::redis_backend::RedisRateLimiter;
use crate::route::BucketKey;

/// Rate limit backend.
pub enum Limiter {
    Memory(MemoryRateLimiter),
    #[cfg(feature = "redis")]
    Redis(Box<RedisRateLimiter>),
}

impl Limiter {
    pub async fn acquire(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_interaction: bool,
    ) -> AcquireResult {
        match self {
            Self::Memory(m) => m.acquire(auth, key, is_interaction).await,
            #[cfg(feature = "redis")]
            Self::Redis(r) => r.acquire(auth, key, is_interaction).await,
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn update_from_response(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        bucket_hash: Option<&str>,
        remaining: Option<u32>,
        limit: Option<u32>,
        reset_after: Option<f64>,
    ) {
        match self {
            Self::Memory(m) => {
                m.update_from_response(auth, key, bucket_hash, remaining, limit, reset_after);
            }
            #[cfg(feature = "redis")]
            Self::Redis(r) => {
                r.update_from_response(auth, key, bucket_hash, remaining, limit, reset_after)
                    .await;
            }
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn handle_rate_limit(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_global: bool,
        is_cloudflare: bool,
        retry_after: Duration,
    ) {
        match self {
            Self::Memory(m) => {
                m.handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after);
            }
            #[cfg(feature = "redis")]
            Self::Redis(r) => {
                r.handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after)
                    .await;
            }
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn report_response(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        status: u16,
        has_via: bool,
    ) -> HealthEvent {
        match self {
            Self::Memory(m) => m.report_response(auth, key, status, has_via),
            #[cfg(feature = "redis")]
            Self::Redis(r) => r.report_response(auth, key, status, has_via).await,
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn is_cloudflare_blocked(&self) -> bool {
        match self {
            Self::Memory(m) => m.is_cloudflare_blocked(),
            #[cfg(feature = "redis")]
            Self::Redis(r) => r.is_cloudflare_blocked().await,
        }
    }

    #[inline]
    pub fn track_invalid(&self) -> u32 {
        match self {
            Self::Memory(m) => m.track_invalid(),
            #[cfg(feature = "redis")]
            Self::Redis(_) => 0,
        }
    }

    #[inline]
    pub fn invalid_count(&self) -> u32 {
        match self {
            Self::Memory(m) => m.invalid_count(),
            #[cfg(feature = "redis")]
            Self::Redis(_) => 0,
        }
    }

    pub fn bucket_count(&self) -> usize {
        match self {
            Self::Memory(m) => m.bucket_count(),
            #[cfg(feature = "redis")]
            Self::Redis(r) => r.bucket_count(),
        }
    }

    pub async fn run_cleanup(&self, interval: Duration, ttl: Duration) {
        match self {
            Self::Memory(m) => m.run_cleanup(interval, ttl).await,
            #[cfg(feature = "redis")]
            Self::Redis(_) => std::future::pending().await,
        }
    }
}
