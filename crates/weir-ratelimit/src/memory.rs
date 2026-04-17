use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::bucket::Bucket;
use crate::global::GlobalRateLimit;
use crate::invalid::InvalidRequestCounter;
use crate::queue::RequestQueue;
use crate::route::BucketKey;

/// Result of a rate limit acquire check.
#[derive(Debug)]
pub enum AcquireResult {
    Allowed,
    CloudflareLimited { retry_after: Duration },
    GlobalLimited { retry_after: Duration },
    BucketLimited { retry_after: Duration },
    QueueTimeout,
}

/// How the request is authenticated, determining which rate limit state to use.
#[derive(Debug, Clone)]
pub enum AuthType {
    Bot(String),
    Bearer(String),
    Webhook,
}

/// Cloudflare-level rate limit state (per proxy IP, shared across all tokens).
#[derive(Debug)]
pub struct CloudflareState {
    blocked_until_ms: AtomicU64,
}

impl Default for CloudflareState {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudflareState {
    pub fn new() -> Self {
        Self {
            blocked_until_ms: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn is_blocked(&self) -> Option<Duration> {
        let until = self.blocked_until_ms.load(Ordering::Acquire);
        if until == 0 {
            return None;
        }
        let now = crate::elapsed_millis();
        if now < until {
            Some(Duration::from_millis(until - now))
        } else {
            None
        }
    }

    pub fn set_blocked(&self, retry_after: Duration) {
        #[allow(clippy::cast_possible_truncation)]
        let until = crate::elapsed_millis() + retry_after.as_millis() as u64;
        self.blocked_until_ms.store(until, Ordering::Release);
    }
}

/// A bucket entry holding both the bucket state and its request queue.
#[derive(Debug)]
pub struct BucketEntry {
    pub bucket: Bucket,
    pub queue: RequestQueue,
}

/// Per-token rate limit state.
pub struct TokenState {
    pub global: GlobalRateLimit,
    /// Maps a route key to the Discord bucket hash learned from response headers.
    pub route_map: DashMap<BucketKey, String>,
    /// Maps `bucket_hash:major_id` to the bucket entry.
    pub buckets: DashMap<String, Arc<BucketEntry>>,
}

impl TokenState {
    pub fn new(global_limit: u32) -> Self {
        Self {
            global: GlobalRateLimit::new(global_limit),
            route_map: DashMap::new(),
            buckets: DashMap::new(),
        }
    }
}

impl std::fmt::Debug for TokenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenState")
            .field("global_limit", &self.global.limit())
            .field("routes", &self.route_map.len())
            .field("buckets", &self.buckets.len())
            .finish()
    }
}

/// In-memory rate limit manager.
pub struct RateLimitManager {
    pub cloudflare: Arc<CloudflareState>,
    pub invalid_requests: InvalidRequestCounter,
    tokens: DashMap<String, Arc<TokenState>>,
    /// Shared state for unauthenticated requests (webhooks with token in URL).
    ip_state: Arc<TokenState>,
    global_limit_default: u32,
    queue_timeout_ms: u64,
}

impl RateLimitManager {
    pub fn new(global_limit_default: u32, queue_timeout_ms: u64) -> Self {
        Self {
            cloudflare: Arc::new(CloudflareState::new()),
            invalid_requests: InvalidRequestCounter::new(),
            tokens: DashMap::new(),
            ip_state: Arc::new(TokenState::new(global_limit_default)),
            global_limit_default,
            queue_timeout_ms,
        }
    }

    /// Get or create the token state for the given auth type.
    fn get_state(&self, auth: &AuthType) -> Arc<TokenState> {
        match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => {
                let entry = self.tokens.entry(id.clone()).or_insert_with(|| {
                    Arc::new(TokenState::new(self.global_limit_default))
                });
                Arc::clone(entry.value())
            }
            AuthType::Webhook => Arc::clone(&self.ip_state),
        }
    }

    /// Get or create a bucket entry for the given hash key.
    fn get_or_create_bucket(&self, state: &TokenState, hash_key: &str) -> Arc<BucketEntry> {
        let entry = state.buckets.entry(hash_key.to_owned()).or_insert_with(|| {
            Arc::new(BucketEntry {
                bucket: Bucket::new(hash_key.to_owned()),
                queue: RequestQueue::new(self.queue_timeout_ms),
            })
        });
        Arc::clone(entry.value())
    }

    /// Check rate limits before forwarding a request to Discord.
    pub async fn acquire(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_interaction: bool,
    ) -> AcquireResult {
        if let Some(retry_after) = self.cloudflare.is_blocked() {
            return AcquireResult::CloudflareLimited { retry_after };
        }

        let state = self.get_state(auth);

        if !is_interaction && !state.global.try_acquire() {
            let retry_after = Duration::from_secs(1);
            return AcquireResult::GlobalLimited { retry_after };
        }

        let hash_key = match state.route_map.get(key) {
            Some(hash) => format!("{}:{}", hash.value(), key.major_id),
            None => return AcquireResult::Allowed, // Unknown route: fail open
        };

        let entry = self.get_or_create_bucket(&state, &hash_key);

        if entry.bucket.try_acquire() {
            return AcquireResult::Allowed;
        }

        if entry.queue.wait().await && entry.bucket.try_acquire() {
            return AcquireResult::Allowed;
        }

        let retry_after = Duration::from_secs(1);
        AcquireResult::BucketLimited { retry_after }
    }

    /// Run periodic cleanup of expired buckets.
    pub async fn run_cleanup(&self, interval: Duration, ttl: Duration) {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            self.cleanup_expired(ttl);
        }
    }

    /// Evict buckets that haven't been used within the given TTL.
    /// Returns the number of evicted entries.
    pub fn cleanup_expired(&self, ttl: Duration) -> u64 {
        let mut evicted = 0u64;
        for entry in &self.tokens {
            let state = entry.value();
            let before = state.buckets.len();
            state.buckets.retain(|_, e| !e.bucket.is_expired(ttl));
            evicted += before.saturating_sub(state.buckets.len()) as u64;
        }

        let before = self.ip_state.buckets.len();
        self.ip_state.buckets.retain(|_, e| !e.bucket.is_expired(ttl));
        evicted += before.saturating_sub(self.ip_state.buckets.len()) as u64;

        evicted
    }

    /// Update rate limit state from Discord response headers.
    pub fn update_from_response(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        bucket_hash: Option<&str>,
        remaining: Option<u32>,
        limit: Option<u32>,
        reset_after: Option<f64>,
    ) {
        let state = self.get_state(auth);

        // Learn bucket hash for this route
        if let Some(hash) = bucket_hash {
            state.route_map.insert(key.clone(), hash.to_owned());

            let hash_key = format!("{hash}:{}", key.major_id);
            let entry = self.get_or_create_bucket(&state, &hash_key);

            if let (Some(rem), Some(lim), Some(reset)) = (remaining, limit, reset_after) {
                entry.bucket.update(rem, lim, reset);

                // Wake queued requests if tokens are available
                if rem > 0 {
                    entry.queue.wake_all();
                }
            }
        }
    }

    /// Handle a 429 response from Discord.
    pub fn handle_rate_limit(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_global: bool,
        is_cloudflare: bool,
        retry_after: Duration,
    ) {
        if is_cloudflare {
            self.cloudflare.set_blocked(retry_after);
            return;
        }

        let state = self.get_state(auth);

        if is_global {
            state.global.set_blocked(retry_after);
            return;
        }

        let hash_key = {
            let hash_ref = state.route_map.get(key);
            match hash_ref {
                Some(h) => format!("{}:{}", h.value(), key.major_id),
                None => return,
            }
        };
        let entry = state.buckets.get(&hash_key).map(|r| Arc::clone(r.value()));
        if let Some(entry) = entry {
            entry.bucket.update(0, entry.bucket.limit(), retry_after.as_secs_f64());
        }
    }
}

impl std::fmt::Debug for RateLimitManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitManager")
            .field("tokens", &self.tokens.len())
            .field("global_limit_default", &self.global_limit_default)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{Method, Resource};

    fn test_key(method: &str, resource: Resource, major_id: &str) -> BucketKey {
        BucketKey {
            method: Method::from_http(method),
            resource,
            major_id: major_id.to_owned(),
            sub_resource: None,
        }
    }

    #[test]
    fn cloudflare_blocks_all() {
        let cf = CloudflareState::new();
        assert!(cf.is_blocked().is_none());

        cf.set_blocked(Duration::from_secs(60));
        assert!(cf.is_blocked().is_some());
    }

    #[tokio::test]
    async fn unknown_route_allows() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        let result = manager.acquire(&auth, &key, false).await;
        assert!(matches!(result, AcquireResult::Allowed));
    }

    #[tokio::test]
    async fn global_limit_enforced() {
        let manager = RateLimitManager::new(2, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::GlobalLimited { .. }
        ));
    }

    #[tokio::test]
    async fn interaction_skips_global() {
        let manager = RateLimitManager::new(1, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("POST", Resource::Interactions, "!");

        // First consumes the global token
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));
        // Second would be blocked by global, but interaction skips it
        assert!(matches!(
            manager.acquire(&auth, &key, true).await,
            AcquireResult::Allowed
        ));
    }

    #[tokio::test]
    async fn cloudflare_blocks_request() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        manager
            .cloudflare
            .set_blocked(Duration::from_secs(60));

        let result = manager.acquire(&auth, &key, false).await;
        assert!(matches!(result, AcquireResult::CloudflareLimited { .. }));
    }

    #[tokio::test]
    async fn bucket_learning_and_enforcement() {
        let manager = RateLimitManager::new(50, 100);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        // First request: unknown route, allowed
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));

        // Simulate Discord response: learn hash, 1 remaining
        manager.update_from_response(&auth, &key, Some("abc"), Some(1), Some(5), Some(5.0));

        // Second request: known route, bucket has 1 remaining
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));

        // Third request: bucket exhausted, will queue and timeout
        let result = manager.acquire(&auth, &key, false).await;
        assert!(matches!(
            result,
            AcquireResult::BucketLimited { .. } | AcquireResult::QueueTimeout
        ));
    }

    #[tokio::test]
    async fn separate_tokens_separate_state() {
        let manager = RateLimitManager::new(1, 5000);
        let auth1 = AuthType::Bot("111".to_owned());
        let auth2 = AuthType::Bot("222".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        // Token 1 exhausts its global limit
        assert!(matches!(
            manager.acquire(&auth1, &key, false).await,
            AcquireResult::Allowed
        ));
        assert!(matches!(
            manager.acquire(&auth1, &key, false).await,
            AcquireResult::GlobalLimited { .. }
        ));

        // Token 2 is independent
        assert!(matches!(
            manager.acquire(&auth2, &key, false).await,
            AcquireResult::Allowed
        ));
    }

    #[tokio::test]
    async fn webhook_uses_ip_state() {
        let manager = RateLimitManager::new(1, 5000);
        let auth = AuthType::Webhook;
        let key = test_key("POST", Resource::Webhooks, "789");

        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::Allowed
        ));
        assert!(matches!(
            manager.acquire(&auth, &key, false).await,
            AcquireResult::GlobalLimited { .. }
        ));
    }

    #[test]
    fn handle_global_429() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        manager.handle_rate_limit(&auth, &key, true, false, Duration::from_secs(5));

        // The global should now be blocked
        let state = manager.get_state(&auth);
        assert!(!state.global.try_acquire());
    }

    #[test]
    fn handle_cloudflare_429() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        manager.handle_rate_limit(&auth, &key, false, true, Duration::from_secs(30));

        assert!(manager.cloudflare.is_blocked().is_some());
    }

    #[test]
    fn cleanup_evicts_expired_buckets() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        manager.update_from_response(&auth, &key, Some("abc"), Some(5), Some(10), Some(1.0));

        let state = manager.get_state(&auth);
        assert_eq!(state.buckets.len(), 1);

        let evicted = manager.cleanup_expired(Duration::ZERO);
        assert_eq!(evicted, 1);
        assert_eq!(state.buckets.len(), 0);
    }

    #[test]
    fn cleanup_preserves_fresh_buckets() {
        let manager = RateLimitManager::new(50, 5000);
        let auth = AuthType::Bot("123".to_owned());
        let key = test_key("GET", Resource::Channels, "456");

        manager.update_from_response(&auth, &key, Some("abc"), Some(5), Some(10), Some(1.0));

        let state = manager.get_state(&auth);
        assert_eq!(state.buckets.len(), 1);

        let evicted = manager.cleanup_expired(Duration::from_secs(3600));
        assert_eq!(evicted, 0);
        assert_eq!(state.buckets.len(), 1);
    }
}
