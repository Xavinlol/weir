use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use dashmap::DashMap;

/// Tracks health of a bot/bearer token via consecutive error streaks.
pub struct TokenHealth {
    error_streak: AtomicU32,
    disabled: AtomicBool,
}

impl TokenHealth {
    pub fn new() -> Self {
        Self {
            error_streak: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }

    pub fn report_success(&self) {
        self.error_streak.store(0, Ordering::Relaxed);
    }

    /// Increment the error streak. Returns `true` only if this call was the
    /// one that flipped `disabled` from false to true (threshold reached).
    pub fn report_error(&self, threshold: u32) -> bool {
        let prev = self.error_streak.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= threshold {
            self.disabled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        } else {
            false
        }
    }

    pub fn enable(&self) {
        self.error_streak.store(0, Ordering::Relaxed);
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
}

/// Tracks health of individual webhooks via consecutive 404 streaks.
pub struct WebhookHealth {
    entries: DashMap<String, WebhookEntry>,
}

impl WebhookHealth {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    #[inline]
    pub fn is_disabled(&self, webhook_id: &str) -> bool {
        self.entries
            .get(webhook_id)
            .is_some_and(|e| e.disabled.load(Ordering::Acquire))
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
            });
        let prev = entry.consecutive_404s.fetch_add(1, Ordering::AcqRel);
        if prev + 1 >= threshold {
            entry
                .disabled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        } else {
            false
        }
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
            entry.disabled.store(false, Ordering::Release);
        }
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
