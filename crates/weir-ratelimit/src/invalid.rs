use std::sync::atomic::{AtomicU64, Ordering};

use crate::{elapsed_millis, pack, unpack};

const WINDOW_MS: u64 = 600_000; // 10 minutes

/// Tracks invalid requests (401/403/429 non-shared) per proxy instance.
///
/// Discord bans IPs that exceed 10,000 invalid requests in a 10-minute window.
pub struct InvalidRequestCounter {
    state: AtomicU64,
}

impl Default for InvalidRequestCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl InvalidRequestCounter {
    pub fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
        }
    }

    /// Record an invalid request. Returns the count in the current window.
    #[inline]
    pub fn track(&self) -> u32 {
        let now = elapsed_millis();
        loop {
            let current = self.state.load(Ordering::Acquire);
            let (window_ms, count) = unpack(current);

            if now.saturating_sub(window_ms) >= WINDOW_MS {
                let new_state = pack(now, 1);
                if self
                    .state
                    .compare_exchange_weak(current, new_state, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return 1;
                }
            } else {
                let new_count = count + 1;
                let new_state = pack(window_ms, new_count);
                if self
                    .state
                    .compare_exchange_weak(current, new_state, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return new_count;
                }
            }
        }
    }

    /// Current count without incrementing. Returns 0 if window expired.
    #[inline]
    pub fn count(&self) -> u32 {
        let now = elapsed_millis();
        let current = self.state.load(Ordering::Acquire);
        let (window_ms, count) = unpack(current);
        if now.saturating_sub(window_ms) >= WINDOW_MS {
            0
        } else {
            count
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_increments_count() {
        let counter = InvalidRequestCounter::new();
        assert_eq!(counter.track(), 1);
        assert_eq!(counter.track(), 2);
        assert_eq!(counter.track(), 3);
        assert_eq!(counter.count(), 3);
    }

    #[test]
    fn count_reads_without_increment() {
        let counter = InvalidRequestCounter::new();
        assert_eq!(counter.count(), 0);
        counter.track();
        assert_eq!(counter.count(), 1);
        assert_eq!(counter.count(), 1);
    }
}
