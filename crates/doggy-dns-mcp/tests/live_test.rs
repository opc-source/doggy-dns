#![allow(clippy::disallowed_methods)]

use doggy_dns_mcp::live::LiveState;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn nacos_zone_signal_reports_cache_size_and_push_freshness() {
    let mut live = LiveState::new();
    live.register_nacos_zone("nacos.local", || 7, || now_unix() - 12);

    let snapshot = live.snapshot();
    let signal = &snapshot["signals"]["nacos.nacos.local"];
    assert_eq!(signal["cachedServices"], 7);
    let ago = signal["lastPushSecondsAgo"].as_i64().unwrap();
    assert!((12..15).contains(&ago), "expected ~12s ago, got {ago}");
}

#[test]
fn nacos_zone_signal_reports_null_when_never_pushed() {
    let mut live = LiveState::new();
    live.register_nacos_zone("svc.local", || 0, || 0);

    let snapshot = live.snapshot();
    assert!(
        snapshot["signals"]["nacos.svc.local"]["lastPushSecondsAgo"].is_null(),
        "never-pushed watchers must report null, not a huge number"
    );
}

#[test]
fn xdp_signal_reports_attach_state() {
    let mut live = LiveState::new();
    live.register_xdp(|| true);

    let snapshot = live.snapshot();
    assert_eq!(snapshot["signals"]["xdp"]["attached"], true);
}
