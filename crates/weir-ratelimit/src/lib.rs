//! Rate limit engine for the Weir Discord REST API proxy.

use std::sync::OnceLock;
use std::time::Instant;

pub mod bucket;
pub mod global;
pub mod memory;
pub mod queue;
pub mod route;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Monotonic milliseconds since first call. Used for lock-free time comparisons.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn elapsed_millis() -> u64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    Instant::now().duration_since(epoch).as_millis() as u64
}

const COUNT_BITS: u64 = 16;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;

/// Pack a millisecond timestamp and a 16-bit counter into a single `u64`.
#[inline]
pub(crate) fn pack(window_ms: u64, count: u32) -> u64 {
    (window_ms << COUNT_BITS) | u64::from(count)
}

/// Unpack a `u64` into (millisecond timestamp, counter).
#[inline]
pub(crate) fn unpack(state: u64) -> (u64, u32) {
    let window_ms = state >> COUNT_BITS;
    #[allow(clippy::cast_possible_truncation)]
    let count = (state & COUNT_MASK) as u32;
    (window_ms, count)
}
