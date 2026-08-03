use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use metrics::{counter, gauge};
use redis::aio::{
    ConnectionLike, ConnectionManager, ConnectionManagerConfig, MultiplexedConnection,
};
use redis::cluster::ClusterClientBuilder;
use redis::cluster_async::ClusterConnection;
use redis::{Client, Cmd, Pipeline, RedisFuture, RedisResult, Script, Value};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::memory::{AcquireResult, AuthType, HealthEvent, ManagerConfig, MemoryRateLimiter};
use crate::route::BucketKey;

const HEALTH_COOLDOWN_MS: u64 = 5 * 60 * 1000;
const CF_BAN_MS: u64 = 60 * 1000;
const TTL_GRACE_MS: u64 = 30 * 1000;
const ROUTE_TTL_MS: u64 = 10 * 60 * 1000;
const REFILL_FALLBACK_MS: u64 = 1000;
const GLOBAL_WINDOW_MS: u64 = 1000;
const WEBHOOK_NAMESPACE: &str = "wh";
const RECONNECT_BACKOFF_MIN_MS: u64 = 1000;
const RECONNECT_BACKOFF_MAX_MS: u64 = 30_000;
const INVALID_WINDOW_MS: u64 = 10 * 60 * 1000;
const DEGRADE_FAILURE_THRESHOLD: u32 = 3;
const CACHE_PRUNE_INTERVAL: Duration = Duration::from_secs(5);
const CLUSTER_RETRY_BUDGET: Duration = Duration::from_secs(3);

// acquire.lua ARGV[5] and its denial reason.
const GLOBAL_CONSUME: u8 = 1;
const GLOBAL_BAN_ONLY: u8 = 2;
const REASON_GLOBAL: i64 = 1;

fn cf_key(prefix: &str) -> String {
    format!("{prefix}cf:blocked_until")
}

fn entropy() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()))
}

fn jitter(base_ms: u64) -> u64 {
    let spread = base_ms / 4;
    base_ms.saturating_sub(spread) + (entropy() % (2 * spread + 1))
}

/// Never returns less than `base_ms`, for waits that are deadlines rather than
/// retry intervals.
fn jitter_up(base_ms: u64) -> u64 {
    base_ms.saturating_add(entropy() % (base_ms / 4 + 1))
}

/// Configuration for the Redis-backed limiter.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub url: String,
    pub cluster_nodes: Vec<String>,
    pub key_prefix: String,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    pub l1_cache_ttl: Duration,
    pub global_limit_default: u32,
    pub queue_timeout: Duration,
    pub token_error_threshold: u32,
    pub webhook_404_threshold: u32,
    pub overrides: HashMap<String, u32>,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_owned(),
            cluster_nodes: Vec::new(),
            key_prefix: "weir:v1:".to_owned(),
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_millis(200),
            l1_cache_ttl: Duration::from_millis(250),
            global_limit_default: 50,
            queue_timeout: Duration::from_secs(5),
            token_error_threshold: 5,
            webhook_404_threshold: 10,
            overrides: HashMap::new(),
        }
    }
}

struct Scripts {
    acquire: Script,
    update_response: Script,
    bucket_update: Script,
    global_429: Script,
    cf_read: Script,
    cf_set_blocked: Script,
    health_record_error: Script,
    health_record_success: Script,
    health_read: Script,
    track_invalid: Script,
}

impl Scripts {
    fn compile() -> Self {
        Self {
            acquire: Script::new(include_str!("scripts/acquire.lua")),
            update_response: Script::new(include_str!("scripts/update_response.lua")),
            bucket_update: Script::new(include_str!("scripts/bucket_update.lua")),
            global_429: Script::new(include_str!("scripts/global_429.lua")),
            cf_read: Script::new(include_str!("scripts/cf_read.lua")),
            cf_set_blocked: Script::new(include_str!("scripts/cf_set_blocked.lua")),
            health_record_error: Script::new(include_str!("scripts/health_record_error.lua")),
            health_record_success: Script::new(include_str!("scripts/health_record_success.lua")),
            health_read: Script::new(include_str!("scripts/health_read.lua")),
            track_invalid: Script::new(include_str!("scripts/track_invalid.lua")),
        }
    }

    async fn load_all(&self, conn: &mut RedisConn) {
        for (name, script) in [
            ("acquire", &self.acquire),
            ("update_response", &self.update_response),
            ("bucket_update", &self.bucket_update),
            ("global_429", &self.global_429),
            ("cf_read", &self.cf_read),
            ("cf_set_blocked", &self.cf_set_blocked),
            ("health_record_error", &self.health_record_error),
            ("health_record_success", &self.health_record_success),
            ("health_read", &self.health_read),
            ("track_invalid", &self.track_invalid),
        ] {
            if let Err(e) = script.load_async(conn).await {
                warn!(error = %e, script = name, "SCRIPT LOAD failed");
            }
        }
    }
}

/// Connection handle that dispatches to either a standalone `ConnectionManager`
/// or a `ClusterConnection`. Both implement `aio::ConnectionLike`; calls forward unchanged.
#[derive(Clone)]
enum RedisConn {
    Standalone(ConnectionManager),
    Cluster(ClusterConnection<MultiplexedConnection>),
}

impl ConnectionLike for RedisConn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Standalone(c) => c.req_packed_command(cmd),
            Self::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Standalone(c) => c.req_packed_commands(cmd, offset, count),
            Self::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Standalone(c) => c.get_db(),
            Self::Cluster(c) => c.get_db(),
        }
    }
}

/// Cached integer with the local `Instant` it was fetched at.
#[derive(Clone, Copy)]
struct CountSnapshot {
    value: u32,
    fetched_at: Instant,
}

impl CountSnapshot {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

fn estimated_now_ms(server_now_ms: u64, fetched_at: Instant) -> u64 {
    let elapsed_ms = u64::try_from(fetched_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    server_now_ms.saturating_add(elapsed_ms)
}

#[derive(Clone, Copy)]
struct CfSnapshot {
    blocked_until_ms: u64,
    server_now_ms: u64,
    fetched_at: Instant,
}

impl CfSnapshot {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }

    fn remaining(self) -> Option<Duration> {
        let now = estimated_now_ms(self.server_now_ms, self.fetched_at);
        (self.blocked_until_ms > now).then(|| Duration::from_millis(self.blocked_until_ms - now))
    }
}

#[derive(Clone, Copy)]
struct HealthSnapshot {
    disabled_at_ms: u64,
    server_now_ms: u64,
    fetched_at: Instant,
}

impl HealthSnapshot {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }

    fn is_disabled(self) -> bool {
        self.disabled_at_ms > 0
            && estimated_now_ms(self.server_now_ms, self.fetched_at)
                < self.disabled_at_ms + HEALTH_COOLDOWN_MS
    }
}

#[derive(Clone)]
struct RouteSnapshot {
    hash: Option<String>,
    fetched_at: Instant,
}

impl RouteSnapshot {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

/// Redis-backed rate limiter.
pub struct RedisRateLimiter {
    conn: RedisConn,
    scripts: Arc<Scripts>,
    config: RedisConfig,
    cf_cache: Mutex<Option<CfSnapshot>>,
    health_cache: DashMap<String, HealthSnapshot>,
    route_cache: DashMap<String, RouteSnapshot>,
    invalid_cache: Mutex<Option<CountSnapshot>>,
    fallback: Arc<MemoryRateLimiter>,
    degraded: Arc<AtomicBool>,
    failures: Arc<AtomicU32>,
    probe_key: Arc<Mutex<String>>,
    reconnect: JoinHandle<()>,
}

impl Drop for RedisRateLimiter {
    fn drop(&mut self) {
        self.reconnect.abort();
    }
}

impl RedisRateLimiter {
    pub async fn new(config: RedisConfig) -> Result<Self> {
        if config.key_prefix.contains(['{', '}']) {
            bail!(
                "redis key_prefix must not contain braces: cluster hash tags depend on them (got {:?})",
                config.key_prefix
            );
        }

        let mut conn = if config.cluster_nodes.is_empty() {
            let client = Client::open(config.url.clone())
                .with_context(|| format!("invalid redis url: {}", config.url))?;
            let cm_config = ConnectionManagerConfig::new()
                .set_connection_timeout(Some(config.connect_timeout))
                .set_response_timeout(Some(config.command_timeout));
            let cm = ConnectionManager::new_with_config(client, cm_config)
                .await
                .context("failed to connect to redis")?;
            RedisConn::Standalone(cm)
        } else {
            let cluster = ClusterClientBuilder::new(config.cluster_nodes.clone())
                .connection_timeout(config.connect_timeout)
                .response_timeout(config.command_timeout)
                .overall_response_timeout(Some(config.command_timeout + CLUSTER_RETRY_BUDGET))
                .build()
                .context("invalid redis cluster config")?;
            let cc = cluster
                .get_async_connection()
                .await
                .context("failed to connect to redis cluster")?;
            RedisConn::Cluster(cc)
        };

        let scripts = Arc::new(Scripts::compile());
        scripts.load_all(&mut conn).await;

        let fallback = Arc::new(MemoryRateLimiter::new(ManagerConfig {
            global_limit_default: config.global_limit_default,
            queue_timeout_ms: u64::try_from(config.queue_timeout.as_millis()).unwrap_or(5000),
            overrides: config.overrides.clone(),
            token_error_threshold: config.token_error_threshold,
            webhook_404_threshold: config.webhook_404_threshold,
        }));
        let degraded = Arc::new(AtomicBool::new(false));
        let failures = Arc::new(AtomicU32::new(0));

        info!(url = %config.url, prefix = %config.key_prefix, "redis backend ready");
        gauge!("weir_redis_fallback_active").set(0.0);

        let probe_key = Arc::new(Mutex::new(cf_key(&config.key_prefix)));
        let reconnect = tokio::spawn(reconnect_loop(
            conn.clone(),
            Arc::clone(&scripts),
            Arc::clone(&degraded),
            Arc::clone(&failures),
            Arc::clone(&probe_key),
        ));

        Ok(Self {
            conn,
            scripts,
            config,
            cf_cache: Mutex::new(None),
            health_cache: DashMap::new(),
            route_cache: DashMap::new(),
            invalid_cache: Mutex::new(None),
            fallback,
            degraded,
            failures,
            probe_key,
            reconnect,
        })
    }

    fn token_id(auth: &AuthType) -> &str {
        match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => id.as_str(),
            AuthType::Webhook => WEBHOOK_NAMESPACE,
        }
    }

    fn cf_key(&self) -> String {
        cf_key(&self.config.key_prefix)
    }

    fn invalid_key(&self) -> String {
        format!("{}invalid:count", self.config.key_prefix)
    }

    fn token_health_key(&self, token_id: &str) -> String {
        format!("{}{{{}}}:health", self.config.key_prefix, token_id)
    }

    fn webhook_health_key(&self, webhook_id: &str) -> String {
        format!("{}wh:{}:health", self.config.key_prefix, webhook_id)
    }

    fn global_key(&self, token_id: &str) -> String {
        format!("{}{{{}}}:global", self.config.key_prefix, token_id)
    }

    fn route_map_key(&self, token_id: &str, key: &BucketKey) -> String {
        format!("{}{{{}}}:route:{}", self.config.key_prefix, token_id, key)
    }

    fn bucket_key(&self, token_id: &str, hash: &str, major_id: &str) -> String {
        format!(
            "{}{{{}}}:bucket:{}:{}",
            self.config.key_prefix, token_id, hash, major_id
        )
    }

    fn bucket_sentinel_key(&self, token_id: &str) -> String {
        format!("{}{{{}}}:bucket:_unknown", self.config.key_prefix, token_id)
    }

    fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    fn observe<T>(&self, kind: &'static str, key: &str, result: RedisResult<T>) -> Option<T> {
        match result {
            Ok(v) => {
                self.failures.store(0, Ordering::Relaxed);
                Some(v)
            }
            Err(e) => {
                warn!(error = %e, op = kind, "redis command failed");
                counter!("weir_redis_errors_total", "kind" => kind).increment(1);
                if self.failures.fetch_add(1, Ordering::AcqRel) + 1 >= DEGRADE_FAILURE_THRESHOLD {
                    self.enter_degraded(kind, key);
                }
                None
            }
        }
    }

    fn enter_degraded(&self, kind: &'static str, key: &str) {
        if let Some(remaining) = self.cf_snapshot().and_then(CfSnapshot::remaining) {
            self.fallback.cloudflare.set_blocked(remaining);
        }
        key.clone_into(&mut self.probe_key.lock().expect("probe_key poisoned"));

        if self.degraded.swap(true, Ordering::AcqRel) {
            return;
        }
        warn!(
            reason = kind,
            "redis degraded; falling back to in-process state"
        );
        gauge!("weir_redis_fallback_active").set(1.0);
    }

    fn cf_snapshot(&self) -> Option<CfSnapshot> {
        *self.cf_cache.lock().expect("cf_cache poisoned")
    }

    fn invalid_snapshot(&self) -> Option<CountSnapshot> {
        *self.invalid_cache.lock().expect("invalid_cache poisoned")
    }

    pub async fn is_cloudflare_blocked(&self) -> bool {
        self.cloudflare_block_remaining().await.is_some()
    }

    async fn cloudflare_block_remaining(&self) -> Option<Duration> {
        if self.is_degraded() {
            return self.fallback.cloudflare.is_blocked();
        }
        if let Some(snap) = self
            .cf_snapshot()
            .filter(|s| s.is_fresh(self.config.l1_cache_ttl))
        {
            return snap.remaining();
        }

        let key = self.cf_key();
        let mut conn = self.conn.clone();
        let result: RedisResult<(u64, u64)> =
            self.scripts.cf_read.key(&key).invoke_async(&mut conn).await;

        let Some((blocked_until_ms, server_now_ms)) = self.observe("cf_read", &key, result) else {
            return self
                .cf_snapshot()
                .and_then(CfSnapshot::remaining)
                .or_else(|| self.fallback.cloudflare.is_blocked());
        };

        let snap = CfSnapshot {
            blocked_until_ms,
            server_now_ms,
            fetched_at: Instant::now(),
        };
        *self.cf_cache.lock().expect("cf_cache poisoned") = Some(snap);

        snap.remaining()
    }

    async fn set_cloudflare_blocked(&self, retry_after: Duration) -> bool {
        let key = self.cf_key();
        let mut conn = self.conn.clone();
        let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);

        let result: RedisResult<(u64, u64)> = self
            .scripts
            .cf_set_blocked
            .key(&key)
            .arg(retry_ms)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        let Some((new_bu, server_now)) = self.observe("cf_set_blocked", &key, result) else {
            return false;
        };

        *self.cf_cache.lock().expect("cf_cache poisoned") = Some(CfSnapshot {
            blocked_until_ms: new_bu,
            server_now_ms: server_now,
            fetched_at: Instant::now(),
        });
        true
    }

    pub async fn track_invalid(&self) -> u32 {
        if self.is_degraded() {
            return self.fallback.track_invalid();
        }
        let key = self.invalid_key();
        let mut conn = self.conn.clone();
        let result: RedisResult<i64> = self
            .scripts
            .track_invalid
            .key(&key)
            .arg(INVALID_WINDOW_MS)
            .invoke_async(&mut conn)
            .await;

        let Some(count) = self.observe("track_invalid", &key, result) else {
            return self.fallback.track_invalid();
        };

        let value = u32::try_from(count).unwrap_or(u32::MAX);
        *self.invalid_cache.lock().expect("invalid_cache poisoned") = Some(CountSnapshot {
            value,
            fetched_at: Instant::now(),
        });
        value
    }

    pub async fn invalid_count(&self) -> u32 {
        if self.is_degraded() {
            return self.fallback.invalid_count();
        }
        if let Some(snap) = self
            .invalid_snapshot()
            .filter(|s| s.is_fresh(self.config.l1_cache_ttl))
        {
            return snap.value;
        }

        let key = self.invalid_key();
        let mut conn = self.conn.clone();
        let result: RedisResult<Option<i64>> =
            redis::cmd("GET").arg(&key).query_async(&mut conn).await;

        let Some(stored) = self.observe("invalid_count", &key, result) else {
            return self.fallback.invalid_count();
        };
        let value = stored.map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));

        *self.invalid_cache.lock().expect("invalid_cache poisoned") = Some(CountSnapshot {
            value,
            fetched_at: Instant::now(),
        });
        value
    }

    async fn record_health_error(&self, key: &str, threshold: u32) -> Option<bool> {
        let mut conn = self.conn.clone();
        let result: RedisResult<i64> = self
            .scripts
            .health_record_error
            .key(key)
            .arg(threshold)
            .arg(HEALTH_COOLDOWN_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        self.health_cache.remove(key);
        let disabled = self.observe("health_record_error", key, result)?;
        Some(disabled == 1)
    }

    async fn record_health_success(&self, key: &str) -> bool {
        let mut conn = self.conn.clone();
        let result: RedisResult<i64> = self
            .scripts
            .health_record_success
            .key(key)
            .arg(HEALTH_COOLDOWN_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        self.observe("health_record_success", key, result).is_some()
    }

    fn health_snapshot(&self, key: &str) -> Option<HealthSnapshot> {
        self.health_cache.get(key).map(|r| *r)
    }

    async fn is_health_disabled(&self, key: &str) -> bool {
        let cached = self.health_snapshot(key);
        if let Some(snap) = cached.filter(|s| s.is_fresh(self.config.l1_cache_ttl)) {
            return snap.is_disabled();
        }

        let mut conn = self.conn.clone();
        let result: RedisResult<(u64, u64)> = self
            .scripts
            .health_read
            .key(key)
            .arg(HEALTH_COOLDOWN_MS)
            .invoke_async(&mut conn)
            .await;

        let Some((disabled_at_ms, server_now_ms)) = self.observe("health_read", key, result) else {
            return cached.is_some_and(HealthSnapshot::is_disabled);
        };

        let snap = HealthSnapshot {
            disabled_at_ms,
            server_now_ms,
            fetched_at: Instant::now(),
        };
        self.health_cache.insert(key.to_owned(), snap);

        snap.is_disabled()
    }

    fn cache_route(&self, route_key: String, hash: Option<String>) {
        self.route_cache.insert(
            route_key,
            RouteSnapshot {
                hash,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Outer `None` is a failed lookup, inner `None` an unlearned route.
    async fn lookup_bucket_hash(&self, token_id: &str, key: &BucketKey) -> Option<Option<String>> {
        let route_key = self.route_map_key(token_id, key);

        let stale = match self.route_cache.get(&route_key) {
            Some(snap) if snap.is_fresh(self.config.l1_cache_ttl) => {
                return Some(snap.hash.clone())
            }
            Some(snap) => Some(snap.hash.clone()),
            None => None,
        };

        let mut conn = self.conn.clone();
        let result = redis::cmd("GET")
            .arg(&route_key)
            .query_async::<Option<String>>(&mut conn)
            .await;

        let Some(hash) = self.observe("route_lookup", &route_key, result) else {
            return stale;
        };
        self.cache_route(route_key, hash.clone());
        Some(hash)
    }

    pub async fn acquire(&self, auth: &AuthType, key: &BucketKey) -> AcquireResult {
        if self.is_degraded() {
            return self.fallback.acquire(auth, key).await;
        }

        if let Some(retry_after) = self.cloudflare_block_remaining().await {
            return AcquireResult::CloudflareLimited { retry_after };
        }

        if key.is_interaction() {
            return AcquireResult::Allowed;
        }

        match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => {
                let hkey = self.token_health_key(id);
                if self.is_health_disabled(&hkey).await {
                    return AcquireResult::TokenDisabled;
                }
            }
            AuthType::Webhook => {
                let hkey = self.webhook_health_key(&key.major_id);
                if self.is_health_disabled(&hkey).await {
                    return AcquireResult::WebhookDisabled;
                }
            }
        }

        let token_id = Self::token_id(auth);
        let bucket_hash = self.lookup_bucket_hash(token_id, key).await.flatten();

        let outcome = self
            .invoke_acquire(
                token_id,
                &key.major_id,
                bucket_hash.as_deref(),
                GLOBAL_CONSUME,
            )
            .await;

        let AcquireOutcome::BucketLimited { retry_after } = outcome else {
            return outcome.into_result();
        };

        let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
        let cap_ms = u64::try_from(self.config.queue_timeout.as_millis()).unwrap_or(u64::MAX);
        tokio::time::sleep(Duration::from_millis(jitter_up(retry_ms).min(cap_ms))).await;

        self.invoke_acquire(
            token_id,
            &key.major_id,
            bucket_hash.as_deref(),
            GLOBAL_BAN_ONLY,
        )
        .await
        .into_result()
    }

    async fn invoke_acquire(
        &self,
        token_id: &str,
        major_id: &str,
        bucket_hash: Option<&str>,
        global_mode: u8,
    ) -> AcquireOutcome {
        let mut conn = self.conn.clone();
        let gkey = self.global_key(token_id);
        let bkey = bucket_hash.map_or_else(
            || self.bucket_sentinel_key(token_id),
            |h| self.bucket_key(token_id, h, major_id),
        );
        let global_limit = self
            .config
            .overrides
            .get(token_id)
            .copied()
            .unwrap_or(self.config.global_limit_default);

        let result: RedisResult<(i64, i64, i64)> = self
            .scripts
            .acquire
            .key(&gkey)
            .key(&bkey)
            .arg(global_limit)
            .arg(GLOBAL_WINDOW_MS)
            .arg(REFILL_FALLBACK_MS)
            .arg(TTL_GRACE_MS)
            .arg(global_mode)
            .invoke_async(&mut conn)
            .await;

        match self.observe("acquire", &bkey, result) {
            Some((1, _, _)) => AcquireOutcome::Allowed,
            Some((_, retry_ms, reason)) => {
                let retry_after = Duration::from_millis(u64::try_from(retry_ms).unwrap_or(0));
                if reason == REASON_GLOBAL {
                    AcquireOutcome::GlobalLimited { retry_after }
                } else {
                    AcquireOutcome::BucketLimited { retry_after }
                }
            }
            None => AcquireOutcome::Error,
        }
    }

    pub async fn update_from_response(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        bucket_hash: Option<&str>,
        remaining: Option<u32>,
        limit: Option<u32>,
        reset_after: Option<f64>,
    ) {
        if key.is_interaction() {
            return;
        }
        if self.is_degraded() {
            self.fallback.update_from_response(
                auth,
                key,
                bucket_hash,
                remaining,
                limit,
                reset_after,
            );
            return;
        }
        let Some(hash) = bucket_hash else { return };

        let token_id = Self::token_id(auth);
        let rkey = self.route_map_key(token_id, key);
        let mut conn = self.conn.clone();

        let stored = if let (Some(rem), Some(lim), Some(reset)) = (remaining, limit, reset_after) {
            let bkey = self.bucket_key(token_id, hash, &key.major_id);

            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let reset_after_ms = (reset.max(0.0) * 1000.0) as u64;

            let result: RedisResult<i64> = self
                .scripts
                .update_response
                .key(&bkey)
                .key(&rkey)
                .arg(rem)
                .arg(reset_after_ms)
                .arg(lim)
                .arg(TTL_GRACE_MS)
                .arg(hash)
                .arg(ROUTE_TTL_MS)
                .invoke_async(&mut conn)
                .await;
            self.observe("update_response", &bkey, result).is_some()
        } else {
            let result: RedisResult<()> = redis::cmd("SET")
                .arg(&rkey)
                .arg(hash)
                .arg("PX")
                .arg(ROUTE_TTL_MS)
                .query_async(&mut conn)
                .await;
            self.observe("route_learn", &rkey, result).is_some()
        };

        if stored {
            self.cache_route(rkey, Some(hash.to_owned()));
        } else {
            self.fallback.update_from_response(
                auth,
                key,
                bucket_hash,
                remaining,
                limit,
                reset_after,
            );
        }
    }

    pub async fn handle_rate_limit(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_global: bool,
        is_cloudflare: bool,
        retry_after: Duration,
    ) {
        if self.is_degraded() {
            self.fallback
                .handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after);
            return;
        }

        let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
        let token_id = Self::token_id(auth);

        let stored = if is_cloudflare {
            self.set_cloudflare_blocked(retry_after).await
        } else if is_global {
            let gkey = self.global_key(token_id);
            let mut conn = self.conn.clone();
            let result: RedisResult<i64> = self
                .scripts
                .global_429
                .key(&gkey)
                .arg(retry_ms)
                .arg(TTL_GRACE_MS)
                .invoke_async(&mut conn)
                .await;
            self.observe("global_429", &gkey, result).is_some()
        } else {
            match self.lookup_bucket_hash(token_id, key).await {
                Some(None) => return,
                Some(Some(hash)) => {
                    let bkey = self.bucket_key(token_id, &hash, &key.major_id);
                    let mut conn = self.conn.clone();
                    let result: RedisResult<i64> = self
                        .scripts
                        .bucket_update
                        .key(&bkey)
                        .arg(0_i64)
                        .arg(retry_ms)
                        .arg(0_i64)
                        .arg(TTL_GRACE_MS)
                        .invoke_async(&mut conn)
                        .await;
                    self.observe("bucket_update", &bkey, result).is_some()
                }
                None => false,
            }
        };

        if !stored {
            self.fallback
                .handle_rate_limit(auth, key, is_global, is_cloudflare, retry_after);
        }
    }

    pub async fn report_response(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        status: u16,
        has_via: bool,
    ) -> HealthEvent {
        if self.is_degraded() {
            return self.fallback.report_response(auth, key, status, has_via);
        }
        if status == 403 && !has_via {
            if !self
                .set_cloudflare_blocked(Duration::from_millis(CF_BAN_MS))
                .await
            {
                return self.fallback.report_response(auth, key, status, has_via);
            }
            return HealthEvent::CloudflareBanned;
        }

        if key.is_interaction() {
            return HealthEvent::None;
        }

        let (hkey, threshold, is_error) = match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => (
                self.token_health_key(id),
                self.config.token_error_threshold,
                has_via && (status == 401 || status == 403),
            ),
            AuthType::Webhook => (
                self.webhook_health_key(&key.major_id),
                self.config.webhook_404_threshold,
                status == 404,
            ),
        };

        if is_error {
            return match self.record_health_error(&hkey, threshold).await {
                Some(true) if matches!(auth, AuthType::Webhook) => HealthEvent::WebhookDisabled,
                Some(true) => HealthEvent::TokenDisabled,
                Some(false) => HealthEvent::None,
                None => self.fallback.report_response(auth, key, status, has_via),
            };
        }

        let resets = match auth {
            AuthType::Bot(_) | AuthType::Bearer(_) => (200..300).contains(&status),
            AuthType::Webhook => true,
        };
        if resets && !self.record_health_success(&hkey).await {
            return self.fallback.report_response(auth, key, status, has_via);
        }

        HealthEvent::None
    }

    pub fn bucket_count(&self) -> usize {
        self.fallback.bucket_count()
    }

    pub async fn run_cleanup(&self, interval: Duration, ttl: Duration) {
        let mut fallback_tick = tokio::time::interval(interval);
        let mut cache_tick = tokio::time::interval(CACHE_PRUNE_INTERVAL);
        fallback_tick.tick().await;
        cache_tick.tick().await;

        loop {
            tokio::select! {
                _ = fallback_tick.tick() => { self.fallback.cleanup_expired(ttl); }
                _ = cache_tick.tick() => {
                    // Keeps stale disabled entries on purpose: they hold a ban
                    // across an outage when the health read cannot be served.
                    self.health_cache.retain(|_, snap| snap.is_disabled());
                    self.route_cache
                        .retain(|_, snap| snap.is_fresh(self.config.l1_cache_ttl));
                }
            }
        }
    }
}

/// Probes one key rather than `PING`, which is broadcast to every cluster primary.
async fn reconnect_loop(
    mut conn: RedisConn,
    scripts: Arc<Scripts>,
    degraded: Arc<AtomicBool>,
    failures: Arc<AtomicU32>,
    probe_key: Arc<Mutex<String>>,
) {
    let mut backoff_ms = RECONNECT_BACKOFF_MIN_MS;
    loop {
        if !degraded.load(Ordering::Acquire) {
            backoff_ms = RECONNECT_BACKOFF_MIN_MS;
            tokio::time::sleep(Duration::from_millis(RECONNECT_BACKOFF_MIN_MS)).await;
            continue;
        }

        let key = probe_key.lock().expect("probe_key poisoned").clone();
        let probe: RedisResult<i64> = redis::cmd("EXISTS").arg(&key).query_async(&mut conn).await;
        if let Err(e) = probe {
            warn!(error = %e, "redis probe failed");
            tokio::time::sleep(Duration::from_millis(jitter(backoff_ms))).await;
            backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
            continue;
        }

        scripts.load_all(&mut conn).await;
        backoff_ms = RECONNECT_BACKOFF_MIN_MS;
        failures.store(0, Ordering::Relaxed);
        if degraded.swap(false, Ordering::AcqRel) {
            info!("redis reconnected, leaving fallback mode");
            gauge!("weir_redis_fallback_active").set(0.0);
            counter!("weir_redis_reconnects_total").increment(1);
        }
    }
}

enum AcquireOutcome {
    Allowed,
    GlobalLimited { retry_after: Duration },
    BucketLimited { retry_after: Duration },
    Error,
}

impl AcquireOutcome {
    fn into_result(self) -> AcquireResult {
        match self {
            Self::Error => {
                counter!("weir_redis_fail_open_total").increment(1);
                AcquireResult::Allowed
            }
            Self::Allowed => AcquireResult::Allowed,
            Self::GlobalLimited { retry_after } => AcquireResult::GlobalLimited { retry_after },
            Self::BucketLimited { retry_after } => AcquireResult::BucketLimited { retry_after },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_up_never_shortens_the_wait() {
        for base in [0, 1, 250, 1000, 30_000] {
            for _ in 0..64 {
                let j = jitter_up(base);
                assert!(j >= base, "jitter_up({base}) = {j}");
                assert!(j <= base + base / 4 + 1, "jitter_up({base}) = {j}");
            }
        }
    }

    #[test]
    fn jitter_stays_within_a_quarter_either_way() {
        for base in [1000, 30_000] {
            for _ in 0..64 {
                let j = jitter(base);
                assert!(j >= base - base / 4 && j <= base + base / 4, "{j}");
            }
        }
    }

    #[test]
    fn cf_snapshot_reports_time_left_on_the_redis_clock() {
        let snap = CfSnapshot {
            blocked_until_ms: 5_000,
            server_now_ms: 4_000,
            fetched_at: Instant::now(),
        };
        assert!(snap.remaining().is_some_and(|d| d.as_millis() <= 1000));

        let expired = CfSnapshot {
            blocked_until_ms: 4_000,
            server_now_ms: 4_000,
            fetched_at: Instant::now(),
        };
        assert!(expired.remaining().is_none());
    }

    #[test]
    fn health_snapshot_disabled_only_within_the_cooldown() {
        let now = 1_000_000;
        let fresh = HealthSnapshot {
            disabled_at_ms: now,
            server_now_ms: now,
            fetched_at: Instant::now(),
        };
        assert!(fresh.is_disabled());

        let elapsed = HealthSnapshot {
            disabled_at_ms: now,
            server_now_ms: now + HEALTH_COOLDOWN_MS,
            fetched_at: Instant::now(),
        };
        assert!(!elapsed.is_disabled());

        let never = HealthSnapshot {
            disabled_at_ms: 0,
            server_now_ms: now,
            fetched_at: Instant::now(),
        };
        assert!(!never.is_disabled());
    }

    #[test]
    fn redis_errors_fail_open() {
        assert!(matches!(
            AcquireOutcome::Error.into_result(),
            AcquireResult::Allowed
        ));
    }
}
