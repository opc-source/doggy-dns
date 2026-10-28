//! Small wall-clock helpers shared across crates.
//!
//! Lives in `doggy-dns-basic` (std-only) so consumers like the Nacos watcher
//! and the control-plane live-state registry can stamp timestamps without
//! re-implementing the same `SystemTime` dance — and without pulling heavier
//! dependencies. Only `SystemTime` is used here: `std::time::Instant` is
//! banned repo-wide in favour of `tokio::time::Instant`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as Unix seconds. A clock before 1970 (only
/// possible with a broken `SystemTime`) clamps to `0`.
pub fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
