//! Retry jitter.
//!
//! Jitter is the driver's job, never the core's: the core has no randomness and
//! must not acquire any. What the core hands over is `base_delay`; the spread
//! around it is added here.
//!
//! The spread is derived from the firing and attempt rather than drawn from an
//! RNG. The goal of jitter is to decorrelate *different* retries so they do not
//! stampede, and a per-firing hash does that. It also avoids a dependency, and
//! leaves the driver reproducible, which makes retry timing testable.

use std::time::Duration;

use ir::{Attempt, FiringId};

/// How far either side of `base_delay` the spread reaches.
pub const SPREAD_PERCENT: u64 = 25;

pub fn jittered(base: Duration, firing: FiringId, attempt: Attempt) -> Duration {
    if base.is_zero() {
        return base;
    }
    let mut hash = firing
        .raw()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(attempt.raw()).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    hash ^= hash >> 31;
    hash = hash.wrapping_mul(0x94D0_49BB_1331_11EB);
    hash ^= hash >> 33;

    let span = 2 * SPREAD_PERCENT + 1;
    let offset = (hash % span) as i64 - SPREAD_PERCENT as i64;
    let nanos = base.as_nanos() as i128;
    let adjusted = nanos + nanos * i128::from(offset) / 100;
    Duration::from_nanos(adjusted.max(0) as u64)
}
