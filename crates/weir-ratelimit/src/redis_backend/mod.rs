use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use dashmap::DashMap;
use metrics::{counter, gauge};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{Client, Script};
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

fn jitter(base_ms: u64) -> u64 {
    use std::time::SystemTime;
    let entropy = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let spread = base_ms / 4;
    base_ms.saturating_sub(spread) + (entropy % (2 * spread + 1))
}

/// Configuration for the Redis-backed limiter.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub url: String,
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
    bucket_only_acquire: Script,
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
            bucket_only_acquire: Script::new(include_str!("scripts/bucket_only_acquire.lua")),
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

    async fn load_all(&self, conn: &mut ConnectionManager) -> Result<()> {
        for (name, script) in [
            ("acquire", &self.acquire),
            ("bucket_only_acquire", &self.bucket_only_acquire),
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
            script
                .load_async(conn)
                .await
                .with_context(|| format!("failed to SCRIPT LOAD {name}"))?;
        }
        Ok(())
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

/// Cached `(value, server_now)` pair with the local `Instant` it was fetched at.
#[derive(Clone, Copy)]
struct Snapshot {
    value_ms: u64,
    server_now_ms: u64,
    fetched_at: Instant,
}

impl Snapshot {
    fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }

    fn estimated_server_now_ms(&self) -> u64 {
        let elapsed_ms = u64::try_from(self.fetched_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.server_now_ms.saturating_add(elapsed_ms)
    }
}

/// Redis-backed rate limiter.
pub struct RedisRateLimiter {
    conn: ConnectionManager,
    scripts: Arc<Scripts>,
    config: RedisConfig,
    cf_cache: Mutex<Option<Snapshot>>,
    health_cache: DashMap<String, Snapshot>,
    invalid_cache: Mutex<Option<CountSnapshot>>,
    fallback: Arc<MemoryRateLimiter>,
    degraded: Arc<AtomicBool>,
}

impl RedisRateLimiter {
    pub async fn new(config: RedisConfig) -> Result<Self> {
        let client = Client::open(config.url.clone())
            .with_context(|| format!("invalid redis url: {}", config.url))?;

        let cm_config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(config.connect_timeout))
            .set_response_timeout(Some(config.command_timeout));

        let mut conn = ConnectionManager::new_with_config(client, cm_config)
            .await
            .context("failed to connect to redis")?;

        let scripts = Arc::new(Scripts::compile());
        scripts.load_all(&mut conn).await?;

        let fallback = Arc::new(MemoryRateLimiter::new(ManagerConfig {
            global_limit_default: config.global_limit_default,
            queue_timeout_ms: u64::try_from(config.queue_timeout.as_millis()).unwrap_or(5000),
            overrides: config.overrides.clone(),
            token_error_threshold: config.token_error_threshold,
            webhook_404_threshold: config.webhook_404_threshold,
        }));
        let degraded = Arc::new(AtomicBool::new(false));

        info!(url = %config.url, prefix = %config.key_prefix, "redis backend ready");
        gauge!("weir_redis_fallback_active").set(0.0);

        let scripts_for_task = Arc::clone(&scripts);
        let degraded_for_task = Arc::clone(&degraded);
        let this = Self {
            conn: conn.clone(),
            scripts,
            config,
            cf_cache: Mutex::new(None),
            health_cache: DashMap::new(),
            invalid_cache: Mutex::new(None),
            fallback,
            degraded: Arc::clone(&degraded),
        };

        tokio::spawn(reconnect_loop(conn, scripts_for_task, degraded_for_task));

        Ok(this)
    }

    fn token_id(auth: &AuthType) -> &str {
        match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => id.as_str(),
            AuthType::Webhook => WEBHOOK_NAMESPACE,
        }
    }

    fn cf_key(&self) -> String {
        format!("{}cf:blocked_until", self.config.key_prefix)
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

    fn mark_degraded(&self, kind: &'static str) {
        if !self.degraded.swap(true, Ordering::AcqRel) {
            warn!(
                reason = kind,
                "redis degraded; falling back to in-process state"
            );
            gauge!("weir_redis_fallback_active").set(1.0);
        }
        counter!("weir_redis_errors_total", "kind" => kind).increment(1);
    }

    pub async fn is_cloudflare_blocked(&self) -> bool {
        if self.is_degraded() {
            return self.fallback.is_cloudflare_blocked();
        }
        if let Some(snap) = self
            .cf_cache
            .lock()
            .expect("cf_cache poisoned")
            .as_ref()
            .copied()
        {
            if snap.is_fresh(self.config.l1_cache_ttl) {
                return snap.value_ms > 0 && snap.estimated_server_now_ms() < snap.value_ms;
            }
        }

        let key = self.cf_key();
        let mut conn = self.conn.clone();
        let result: Result<(u64, u64), _> =
            self.scripts.cf_read.key(&key).invoke_async(&mut conn).await;

        let (blocked_until_ms, server_now_ms) = match result {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "redis cf_read failed");
                self.mark_degraded("cf_read");
                return self.fallback.is_cloudflare_blocked();
            }
        };

        let snap = Snapshot {
            value_ms: blocked_until_ms,
            server_now_ms,
            fetched_at: Instant::now(),
        };
        *self.cf_cache.lock().expect("cf_cache poisoned") = Some(snap);

        blocked_until_ms > 0 && server_now_ms < blocked_until_ms
    }

    async fn set_cloudflare_blocked(&self, retry_after: Duration) {
        let key = self.cf_key();
        let mut conn = self.conn.clone();
        let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);

        let result: Result<(u64, u64), _> = self
            .scripts
            .cf_set_blocked
            .key(&key)
            .arg(retry_ms)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok((new_bu, server_now)) => {
                let snap = Snapshot {
                    value_ms: new_bu,
                    server_now_ms: server_now,
                    fetched_at: Instant::now(),
                };
                *self.cf_cache.lock().expect("cf_cache poisoned") = Some(snap);
            }
            Err(e) => {
                warn!(error = %e, "redis cf_set_blocked failed");
                self.mark_degraded("cf_set_blocked");
            }
        }
    }

    pub async fn track_invalid(&self) -> u32 {
        if self.is_degraded() {
            return self.fallback.track_invalid();
        }
        let key = self.invalid_key();
        let mut conn = self.conn.clone();
        let result: Result<i64, _> = self
            .scripts
            .track_invalid
            .key(&key)
            .arg(INVALID_WINDOW_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(count) => {
                let value = u32::try_from(count).unwrap_or(u32::MAX);
                *self.invalid_cache.lock().expect("invalid_cache poisoned") = Some(CountSnapshot {
                    value,
                    fetched_at: Instant::now(),
                });
                value
            }
            Err(e) => {
                warn!(error = %e, "redis track_invalid failed");
                self.mark_degraded("track_invalid");
                self.fallback.track_invalid()
            }
        }
    }

    pub async fn invalid_count(&self) -> u32 {
        if self.is_degraded() {
            return self.fallback.invalid_count();
        }
        if let Some(snap) = self
            .invalid_cache
            .lock()
            .expect("invalid_cache poisoned")
            .as_ref()
            .copied()
        {
            if snap.is_fresh(self.config.l1_cache_ttl) {
                return snap.value;
            }
        }

        let key = self.invalid_key();
        let mut conn = self.conn.clone();
        let result: Result<Option<i64>, _> =
            redis::cmd("GET").arg(&key).query_async(&mut conn).await;

        let value = match result {
            Ok(Some(v)) => u32::try_from(v).unwrap_or(u32::MAX),
            Ok(None) => 0,
            Err(e) => {
                warn!(error = %e, "redis invalid_count read failed");
                self.mark_degraded("invalid_count");
                return self.fallback.invalid_count();
            }
        };

        *self.invalid_cache.lock().expect("invalid_cache poisoned") = Some(CountSnapshot {
            value,
            fetched_at: Instant::now(),
        });
        value
    }

    async fn record_health_error(&self, key: &str, threshold: u32) -> bool {
        let mut conn = self.conn.clone();
        let result: Result<i64, _> = self
            .scripts
            .health_record_error
            .key(key)
            .arg(threshold)
            .arg(HEALTH_COOLDOWN_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(v) => {
                self.health_cache.remove(key);
                v == 1
            }
            Err(e) => {
                warn!(error = %e, "redis health_record_error failed");
                self.mark_degraded("health_record_error");
                false
            }
        }
    }

    async fn record_health_success(&self, key: &str) {
        let mut conn = self.conn.clone();
        let result: Result<i64, _> = self
            .scripts
            .health_record_success
            .key(key)
            .arg(HEALTH_COOLDOWN_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok(_) => {
                self.health_cache.remove(key);
            }
            Err(e) => {
                warn!(error = %e, "redis health_record_success failed");
                self.mark_degraded("health_record_success");
            }
        }
    }

    async fn is_health_disabled(&self, key: &str) -> bool {
        if let Some(snap) = self.health_cache.get(key).map(|r| *r) {
            if snap.is_fresh(self.config.l1_cache_ttl) {
                return snap.value_ms > 0
                    && snap.estimated_server_now_ms() < snap.value_ms + HEALTH_COOLDOWN_MS;
            }
        }

        let mut conn = self.conn.clone();
        let result: Result<(u64, u64), _> = self
            .scripts
            .health_read
            .key(key)
            .arg(HEALTH_COOLDOWN_MS)
            .invoke_async(&mut conn)
            .await;

        let (disabled_at, server_now) = match result {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "redis health_read failed");
                self.mark_degraded("health_read");
                return false;
            }
        };

        self.health_cache.insert(
            key.to_owned(),
            Snapshot {
                value_ms: disabled_at,
                server_now_ms: server_now,
                fetched_at: Instant::now(),
            },
        );

        disabled_at > 0 && server_now < disabled_at + HEALTH_COOLDOWN_MS
    }

    async fn lookup_bucket_hash(&self, token_id: &str, key: &BucketKey) -> Option<String> {
        let route_key = self.route_map_key(token_id, key);
        let mut conn = self.conn.clone();
        match redis::cmd("GET")
            .arg(&route_key)
            .query_async::<Option<String>>(&mut conn)
            .await
        {
            Ok(opt) => opt,
            Err(e) => {
                warn!(error = %e, "redis route lookup failed");
                self.mark_degraded("route_lookup");
                None
            }
        }
    }

    pub async fn acquire(
        &self,
        auth: &AuthType,
        key: &BucketKey,
        is_interaction: bool,
    ) -> AcquireResult {
        if self.is_degraded() {
            return self.fallback.acquire(auth, key, is_interaction).await;
        }

        if self.is_cloudflare_blocked().await {
            return AcquireResult::CloudflareLimited {
                retry_after: Duration::from_millis(CF_BAN_MS),
            };
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
        let bucket_hash = self.lookup_bucket_hash(token_id, key).await;

        let outcome = self
            .invoke_acquire(
                token_id,
                &key.major_id,
                bucket_hash.as_deref(),
                is_interaction,
            )
            .await;

        match outcome {
            AcquireOutcome::Allowed | AcquireOutcome::Error => AcquireResult::Allowed,
            AcquireOutcome::GlobalLimited { retry_after } => {
                AcquireResult::GlobalLimited { retry_after }
            }
            AcquireOutcome::BucketLimited { retry_after } => {
                let wait = retry_after.min(self.config.queue_timeout);
                tokio::time::sleep(wait).await;
                let Some(hash) = bucket_hash.as_deref() else {
                    return AcquireResult::Allowed;
                };
                // bucket_only_acquire returns only Allowed / BucketLimited / Error.
                match self
                    .bucket_only_acquire(token_id, &key.major_id, hash)
                    .await
                {
                    AcquireOutcome::Allowed => AcquireResult::Allowed,
                    AcquireOutcome::BucketLimited { retry_after } => {
                        AcquireResult::BucketLimited { retry_after }
                    }
                    AcquireOutcome::Error | AcquireOutcome::GlobalLimited { .. } => {
                        AcquireResult::BucketLimited {
                            retry_after: Duration::from_secs(1),
                        }
                    }
                }
            }
        }
    }

    async fn invoke_acquire(
        &self,
        token_id: &str,
        major_id: &str,
        bucket_hash: Option<&str>,
        is_interaction: bool,
    ) -> AcquireOutcome {
        let mut conn = self.conn.clone();

        if is_interaction {
            let Some(hash) = bucket_hash else {
                return AcquireOutcome::Allowed;
            };
            return self.bucket_only_acquire(token_id, major_id, hash).await;
        }

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

        let result: Result<(i64, i64), _> = self
            .scripts
            .acquire
            .key(&gkey)
            .key(&bkey)
            .arg(global_limit)
            .arg(GLOBAL_WINDOW_MS)
            .arg(REFILL_FALLBACK_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok((1, _)) => AcquireOutcome::Allowed,
            Ok((_, retry_ms)) => {
                let retry_after = Duration::from_millis(u64::try_from(retry_ms).unwrap_or(0));
                if bucket_hash.is_none() {
                    AcquireOutcome::GlobalLimited { retry_after }
                } else {
                    AcquireOutcome::BucketLimited { retry_after }
                }
            }
            Err(e) => {
                warn!(error = %e, "redis acquire failed");
                self.mark_degraded("acquire");
                AcquireOutcome::Error
            }
        }
    }

    async fn bucket_only_acquire(
        &self,
        token_id: &str,
        major_id: &str,
        bucket_hash: &str,
    ) -> AcquireOutcome {
        let mut conn = self.conn.clone();
        let bkey = self.bucket_key(token_id, bucket_hash, major_id);
        let result: Result<(i64, i64), _> = self
            .scripts
            .bucket_only_acquire
            .key(&bkey)
            .arg(REFILL_FALLBACK_MS)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;

        match result {
            Ok((1, _)) => AcquireOutcome::Allowed,
            Ok((_, retry_ms)) => AcquireOutcome::BucketLimited {
                retry_after: Duration::from_millis(u64::try_from(retry_ms).unwrap_or(0)),
            },
            Err(e) => {
                warn!(error = %e, "redis bucket_only_acquire failed");
                self.mark_degraded("bucket_only_acquire");
                AcquireOutcome::Error
            }
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
        let (Some(rem), Some(lim), Some(reset)) = (remaining, limit, reset_after) else {
            return;
        };

        let token_id = Self::token_id(auth);
        let bkey = self.bucket_key(token_id, hash, &key.major_id);
        let rkey = self.route_map_key(token_id, key);

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let reset_after_ms = (reset.max(0.0) * 1000.0) as u64;

        let mut conn = self.conn.clone();
        let result: Result<i64, _> = self
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

        if let Err(e) = result {
            warn!(error = %e, "redis update_response failed");
            self.mark_degraded("update_response");
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

        if is_cloudflare {
            self.set_cloudflare_blocked(retry_after).await;
            return;
        }

        let token_id = Self::token_id(auth);

        if is_global {
            let gkey = self.global_key(token_id);
            let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
            let mut conn = self.conn.clone();
            let result: Result<i64, _> = self
                .scripts
                .global_429
                .key(&gkey)
                .arg(retry_ms)
                .arg(TTL_GRACE_MS)
                .invoke_async(&mut conn)
                .await;
            if let Err(e) = result {
                warn!(error = %e, "redis global_429 failed");
                self.mark_degraded("global_429");
            }
            return;
        }

        let Some(hash) = self.lookup_bucket_hash(token_id, key).await else {
            return;
        };
        let bkey = self.bucket_key(token_id, &hash, &key.major_id);
        let retry_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);

        let mut conn = self.conn.clone();
        let result: Result<i64, _> = self
            .scripts
            .bucket_update
            .key(&bkey)
            .arg(0_i64)
            .arg(retry_ms)
            .arg(0_i64)
            .arg(TTL_GRACE_MS)
            .invoke_async(&mut conn)
            .await;
        if let Err(e) = result {
            warn!(error = %e, "redis bucket_update failed");
            self.mark_degraded("bucket_update");
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
            self.set_cloudflare_blocked(Duration::from_millis(CF_BAN_MS))
                .await;
            return HealthEvent::CloudflareBanned;
        }

        match auth {
            AuthType::Bot(id) | AuthType::Bearer(id) => {
                let hkey = self.token_health_key(id);
                if (200..300).contains(&status) {
                    self.record_health_success(&hkey).await;
                } else if has_via
                    && (status == 401 || status == 403)
                    && self
                        .record_health_error(&hkey, self.config.token_error_threshold)
                        .await
                {
                    return HealthEvent::TokenDisabled;
                }
            }
            AuthType::Webhook => {
                let hkey = self.webhook_health_key(&key.major_id);
                if status == 404 {
                    if self
                        .record_health_error(&hkey, self.config.webhook_404_threshold)
                        .await
                    {
                        return HealthEvent::WebhookDisabled;
                    }
                } else {
                    self.record_health_success(&hkey).await;
                }
            }
        }

        HealthEvent::None
    }

    pub fn bucket_count(&self) -> usize {
        if self.is_degraded() {
            return self.fallback.bucket_count();
        }
        0
    }
}

async fn reconnect_loop(
    mut conn: ConnectionManager,
    scripts: Arc<Scripts>,
    degraded: Arc<AtomicBool>,
) {
    let mut backoff_ms = RECONNECT_BACKOFF_MIN_MS;
    loop {
        tokio::time::sleep(Duration::from_millis(jitter(backoff_ms))).await;
        if !degraded.load(Ordering::Acquire) {
            backoff_ms = RECONNECT_BACKOFF_MIN_MS;
            continue;
        }
        let Ok(_) = redis::cmd("PING").query_async::<String>(&mut conn).await else {
            backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
            continue;
        };
        if let Err(e) = scripts.load_all(&mut conn).await {
            warn!(error = %e, "redis reconnect: SCRIPT LOAD failed");
            backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
            continue;
        }
        backoff_ms = RECONNECT_BACKOFF_MIN_MS;
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
