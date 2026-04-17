use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::elapsed_millis;

#[derive(Debug)]
pub struct Bucket {
    pub hash: String,
    remaining: AtomicU32,
    limit: AtomicU32,
    reset_at_ms: AtomicU64,
    last_used_ms: AtomicU64,
}

impl Bucket {
    pub fn new(hash: String) -> Self {
        Self {
            hash,
            remaining: AtomicU32::new(1),
            limit: AtomicU32::new(1),
            reset_at_ms: AtomicU64::new(u64::MAX),
            last_used_ms: AtomicU64::new(elapsed_millis()),
        }
    }

    /// Try to consume a rate limit token. Returns true if allowed.
    #[inline]
    pub fn try_acquire(&self) -> bool {
        let now = elapsed_millis();
        let reset_at = self.reset_at_ms.load(Ordering::Acquire);

        if now >= reset_at
            && self
                .reset_at_ms
                .compare_exchange(reset_at, u64::MAX, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let limit = self.limit.load(Ordering::Relaxed);
            self.remaining.store(limit, Ordering::Release);
        }

        loop {
            let current = self.remaining.load(Ordering::Acquire);
            if current == 0 {
                return false;
            }
            if self
                .remaining
                .compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                self.last_used_ms.store(now, Ordering::Relaxed);
                return true;
            }
        }
    }

    /// Update bucket state from Discord response headers.
    pub fn update(&self, remaining: u32, limit: u32, reset_after_secs: f64) {
        let now = elapsed_millis();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let reset_at = now + (reset_after_secs * 1000.0) as u64;
        self.limit.store(limit, Ordering::Release);
        self.remaining.store(remaining, Ordering::Release);
        self.reset_at_ms.store(reset_at, Ordering::Release);
        self.last_used_ms.store(now, Ordering::Relaxed);
    }

    #[inline]
    pub fn limit(&self) -> u32 {
        self.limit.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn is_expired(&self, ttl: Duration) -> bool {
        let now = elapsed_millis();
        let last = self.last_used_ms.load(Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)]
        let ttl_ms = ttl.as_millis() as u64;
        now.saturating_sub(last) >= ttl_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_first_request() {
        let bucket = Bucket::new("test".into());
        assert!(bucket.try_acquire());
    }

    #[test]
    fn blocks_when_exhausted() {
        let bucket = Bucket::new("test".into());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn update_refreshes_tokens() {
        let bucket = Bucket::new("test".into());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());

        bucket.update(5, 10, 1.0);
        for _ in 0..5 {
            assert!(bucket.try_acquire());
        }
        assert!(!bucket.try_acquire());
    }
}
