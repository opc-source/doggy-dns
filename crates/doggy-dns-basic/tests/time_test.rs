#![allow(clippy::disallowed_methods)]

use std::time::{SystemTime, UNIX_EPOCH};

fn wall_clock_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn unix_now_secs_tracks_the_wall_clock() {
    let before = wall_clock_secs();
    let utility = doggy_dns_basic::time::unix_now_secs();
    let after = wall_clock_secs();

    assert!(
        (before..=after).contains(&utility),
        "{utility} outside {before}..={after}"
    );
}

#[test]
fn unix_now_secs_is_monotonic_across_calls() {
    let first = doggy_dns_basic::time::unix_now_secs();
    let second = doggy_dns_basic::time::unix_now_secs();
    assert!(second >= first);
}
