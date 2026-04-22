use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::elapsed_millis;

/// Refill window used when no Discord response has set a real `reset_at_ms`.
const REFILL_FALLBACK_MS: u64 = 1000;

/// Pack `remaining` (low 32) and `epoch` (high 32) into a single `u64`.
#[inline]
const fn pack_state(remaining: u32, epoch: u32) -> u64 {
    ((epoch as u64) << 32) | (remaining as u64)
}

/// Unpack a `u64` into `(remaining, epoch)`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
const fn unpack_state(state: u64) -> (u32, u32) {
    (state as u32, (state >> 32) as u32)
}

#[derive(Debug)]
pub struct Bucket {
    pub hash: String,
    state: AtomicU64,
    limit: AtomicU32,
    reset_at_ms: AtomicU64,
    last_used_ms: AtomicU64,
}

impl Bucket {
    pub fn new(hash: String) -> Self {
        Self {
            hash,
            state: AtomicU64::new(pack_state(1, 0)),
            limit: AtomicU32::new(1),
            reset_at_ms: AtomicU64::new(0),
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
                .compare_exchange(
                    reset_at,
                    now + REFILL_FALLBACK_MS,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            let observed = self.state.load(Ordering::Acquire);
            let (_, epoch) = unpack_state(observed);
            let limit = self.limit.load(Ordering::Relaxed);
            let _ = self.state.compare_exchange(
                observed,
                pack_state(limit, epoch.wrapping_add(1)),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }

        let acquired = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                let (remaining, epoch) = unpack_state(current);
                if remaining == 0 {
                    None
                } else {
                    Some(pack_state(remaining - 1, epoch))
                }
            })
            .is_ok();

        if acquired {
            self.last_used_ms.store(now, Ordering::Relaxed);
        }
        acquired
    }

    /// Update bucket state from Discord response headers.
    pub fn update(&self, remaining: u32, limit: u32, reset_after_secs: f64) {
        let now = elapsed_millis();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let reset_at = now + (reset_after_secs * 1000.0) as u64;
        self.limit.store(limit, Ordering::Release);
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                let (_, epoch) = unpack_state(current);
                Some(pack_state(remaining, epoch.wrapping_add(1)))
            });
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

    #[test]
    fn self_heals_without_update() {
        // Drained bucket must refill after the fallback window even if no `update()` arrives.
        let bucket = Bucket::new("test".into());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());

        std::thread::sleep(Duration::from_millis(REFILL_FALLBACK_MS + 50));
        assert!(bucket.try_acquire());
    }
}
