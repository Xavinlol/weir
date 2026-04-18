use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

#[allow(clippy::cast_possible_truncation)]
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Tracks health of a bot/bearer token via consecutive error streaks.
pub struct TokenHealth {
    error_streak: AtomicU32,
    disabled: AtomicBool,
    disabled_at: AtomicU64,
    cooldown_ms: u64,
}

impl TokenHealth {
    pub fn new() -> Self {
        Self::with_cooldown(Duration::from_mins(5))
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            error_streak: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            disabled_at: AtomicU64::new(0),
            cooldown_ms: cooldown.as_millis() as u64,
        }
    }

    #[inline]
    pub fn is_disabled(&self) -> bool {
        if !self.disabled.load(Ordering::Acquire) {
            return false;
        }
        // Auto-recover after cooldown
        let at = self.disabled_at.load(Ordering::Acquire);
        if at > 0 && now_millis().saturating_sub(at) >= self.cooldown_ms {
            self.enable();
            return false;
        }
        true
    }

    pub fn report_success(&self) {
        self.error_streak.store(0, Ordering::Relaxed);
    }

    /// Increment the error streak. Returns `true` only if this call was the
    /// one that flipped `disabled` from false to true (threshold reached).
    pub fn report_error(&self, threshold: u32) -> bool {
        let prev = self.error_streak.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= threshold
            && self
                .disabled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.disabled_at.store(now_millis(), Ordering::Release);
            return true;
        }
        false
    }

    pub fn enable(&self) {
        self.error_streak.store(0, Ordering::Relaxed);
        self.disabled_at.store(0, Ordering::Relaxed);
        self.disabled.store(false, Ordering::Release);
    }
}

impl Default for TokenHealth {
    fn default() -> Self {
        Self::new()
    }
}

struct WebhookEntry {
    consecutive_404s: AtomicU32,
    disabled: AtomicBool,
    disabled_at: AtomicU64,
}

/// Tracks health of individual webhooks via consecutive 404 streaks.
pub struct WebhookHealth {
    entries: DashMap<String, WebhookEntry>,
    cooldown_ms: u64,
}

impl WebhookHealth {
    pub fn new() -> Self {
        Self::with_cooldown(Duration::from_mins(5))
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn with_cooldown(cooldown: Duration) -> Self {
        Self {
            entries: DashMap::new(),
            cooldown_ms: cooldown.as_millis() as u64,
        }
    }

    #[inline]
    pub fn is_disabled(&self, webhook_id: &str) -> bool {
        if let Some(e) = self.entries.get(webhook_id) {
            if !e.disabled.load(Ordering::Acquire) {
                return false;
            }
            // Auto-recover after cooldown
            let at = e.disabled_at.load(Ordering::Acquire);
            if at > 0 && now_millis().saturating_sub(at) >= self.cooldown_ms {
                drop(e); // Release DashMap ref before mutating
                self.enable(webhook_id);
                return false;
            }
            return true;
        }
        false
    }

    /// Increment the 404 streak. Returns `true` only if this call was the
    /// one that flipped `disabled` from false to true (threshold reached).
    pub fn report_404(&self, webhook_id: &str, threshold: u32) -> bool {
        let entry = self
            .entries
            .entry(webhook_id.to_owned())
            .or_insert_with(|| WebhookEntry {
                consecutive_404s: AtomicU32::new(0),
                disabled: AtomicBool::new(false),
                disabled_at: AtomicU64::new(0),
            });
        let prev = entry.consecutive_404s.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= threshold
            && entry
                .disabled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            entry.disabled_at.store(now_millis(), Ordering::Release);
            return true;
        }
        false
    }

    pub fn report_success(&self, webhook_id: &str) {
        if let Some(entry) = self.entries.get(webhook_id) {
            entry.consecutive_404s.store(0, Ordering::Relaxed);
            entry.disabled.store(false, Ordering::Release);
        }
    }

    pub fn enable(&self, webhook_id: &str) {
        if let Some(entry) = self.entries.get(webhook_id) {
            entry.consecutive_404s.store(0, Ordering::Relaxed);
            entry.disabled_at.store(0, Ordering::Relaxed);
            entry.disabled.store(false, Ordering::Release);
        }
    }

    /// Remove idle entries (not disabled, zero error streak) to reclaim memory.
    pub fn cleanup_idle(&self) {
        self.entries.retain(|_, entry| {
            entry.disabled.load(Ordering::Relaxed)
                || entry.consecutive_404s.load(Ordering::Relaxed) > 0
        });
    }
}

impl Default for WebhookHealth {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_health_disables_after_threshold() {
        let health = TokenHealth::new();
        assert!(!health.is_disabled());

        assert!(!health.report_error(3));
        assert!(!health.report_error(3));
        assert!(health.report_error(3));
        assert!(health.is_disabled());

        // Second thread hitting threshold does NOT get true
        assert!(!health.report_error(3));
    }

    #[test]
    fn token_health_success_resets_streak() {
        let health = TokenHealth::new();
        health.report_error(5);
        health.report_error(5);
        health.report_success();
        // Streak reset, need 5 more to disable
        for _ in 0..4 {
            assert!(!health.report_error(5));
        }
        assert!(health.report_error(5));
    }

    #[test]
    fn token_health_enable_recovers() {
        let health = TokenHealth::new();
        for _ in 0..3 {
            health.report_error(3);
        }
        assert!(health.is_disabled());

        health.enable();
        assert!(!health.is_disabled());
    }

    #[test]
    fn webhook_health_disables_after_threshold() {
        let health = WebhookHealth::new();
        assert!(!health.is_disabled("wh1"));

        assert!(!health.report_404("wh1", 3));
        assert!(!health.report_404("wh1", 3));
        assert!(health.report_404("wh1", 3));
        assert!(health.is_disabled("wh1"));

        // Other webhooks unaffected
        assert!(!health.is_disabled("wh2"));
    }

    #[test]
    fn webhook_health_success_resets() {
        let health = WebhookHealth::new();
        health.report_404("wh1", 5);
        health.report_404("wh1", 5);
        health.report_success("wh1");
        // Streak reset
        assert!(!health.report_404("wh1", 5));
    }

    #[test]
    fn webhook_health_enable_recovers() {
        let health = WebhookHealth::new();
        for _ in 0..3 {
            health.report_404("wh1", 3);
        }
        assert!(health.is_disabled("wh1"));

        health.enable("wh1");
        assert!(!health.is_disabled("wh1"));
    }
}
