use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::{elapsed_millis, pack, unpack};

/// Tracks the global rate limit for a single bot token.
#[derive(Debug)]
pub struct GlobalRateLimit {
    limit: AtomicU32,
    /// Packed (`window_start_ms`, count): upper 48 bits window, lower 16 bits count.
    state: AtomicU64,
    blocked_until_ms: AtomicU64,
}

impl GlobalRateLimit {
    pub fn new(limit: u32) -> Self {
        Self {
            limit: AtomicU32::new(limit),
            state: AtomicU64::new(pack(elapsed_millis(), 0)),
            blocked_until_ms: AtomicU64::new(0),
        }
    }

    /// Try to consume a global rate limit token. Returns true if allowed.
    #[inline]
    pub fn try_acquire(&self) -> bool {
        let now = elapsed_millis();

        let blocked_until = self.blocked_until_ms.load(Ordering::Acquire);
        if blocked_until > 0 && now < blocked_until {
            return false;
        }

        let limit = self.limit.load(Ordering::Relaxed);

        loop {
            let current = self.state.load(Ordering::Acquire);
            let (window_ms, count) = unpack(current);

            if now.saturating_sub(window_ms) >= 1000 {
                let new_state = pack(now, 1);
                if self
                    .state
                    .compare_exchange_weak(current, new_state, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
            } else if count < limit {
                let new_state = pack(window_ms, count + 1);
                if self
                    .state
                    .compare_exchange_weak(current, new_state, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
            } else {
                return false;
            }
        }
    }

    pub fn set_blocked(&self, retry_after: Duration) {
        #[allow(clippy::cast_possible_truncation)]
        let until = elapsed_millis() + retry_after.as_millis() as u64;
        self.blocked_until_ms.store(until, Ordering::Release);
    }

    #[inline]
    pub fn set_limit(&self, limit: u32) {
        self.limit.store(limit, Ordering::Release);
    }

    #[inline]
    pub fn limit(&self) -> u32 {
        self.limit.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_requests_within_limit() {
        let rl = GlobalRateLimit::new(5);
        for _ in 0..5 {
            assert!(rl.try_acquire());
        }
        assert!(!rl.try_acquire());
    }

    #[test]
    fn blocks_when_globally_limited() {
        let rl = GlobalRateLimit::new(50);
        rl.set_blocked(Duration::from_secs(10));
        assert!(!rl.try_acquire());
    }

    #[test]
    fn limit_can_be_updated() {
        let rl = GlobalRateLimit::new(50);
        assert_eq!(rl.limit(), 50);
        rl.set_limit(500);
        assert_eq!(rl.limit(), 500);
    }
}
