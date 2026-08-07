//! Wall-clock time, in the one form the whole system uses.
//!
//! This lived in the schedule plugin until the crate split, because
//! that is where it was first needed. Nine core modules had come to
//! depend on it, which made core depend on a plugin: the single thing
//! standing between the design and a clean split. It is here now
//! because a timestamp belongs to nobody in particular.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch. Zero conventionally means "never".
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
