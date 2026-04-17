//! Rate limit engine for the Weir Discord REST API proxy.

use std::sync::OnceLock;
use std::time::Instant;

pub mod bucket;
pub mod global;
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
