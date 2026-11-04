#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::time::unix_now_secs as now_unix;
use doggy_dns_control::live::{LiveState, nacos_zone_signal};

#[test]
fn nacos_zone_signal_reports_cache_size_and_push_freshness() {
    let live = LiveState::new();
    live.register_nacos_zone("nacos.local", || 7, || now_unix() - 12);

    let snapshot = live.snapshot();
    let signal = &snapshot["signals"]["nacos.nacos.local"];
    assert_eq!(signal["cachedServices"], 7);
    let ago = signal["lastPushSecondsAgo"].as_i64().unwrap();
    assert!((12..15).contains(&ago), "expected ~12s ago, got {ago}");
}

#[test]
fn nacos_zone_signal_reports_null_when_never_pushed() {
    let live = LiveState::new();
    live.register_nacos_zone("svc.local", || 0, || 0);

    let snapshot = live.snapshot();
    assert!(
        snapshot["signals"]["nacos.svc.local"]["lastPushSecondsAgo"].is_null(),
        "never-pushed watchers must report null, not a huge number"
    );
}

#[test]
fn xdp_signal_reports_attach_state() {
    let live = LiveState::new();
    live.register_xdp(|| true);

    let snapshot = live.snapshot();
    assert_eq!(snapshot["signals"]["xdp"]["attached"], true);
}

#[test]
fn replace_nacos_zones_swaps_only_the_zone_signals() {
    // A reload swaps the zones it owns while the XDP signal — registered once
    // at startup — has to survive it. Without this, `/readyz` would keep
    // reporting the caches of watchers nothing owns any more.
    let live = LiveState::new();
    live.register_nacos_zone("old.local", || 5, || 0);
    live.register_xdp(|| false);

    live.replace_nacos_zones(vec![
        nacos_zone_signal("new.local", || 1, || now_unix() - 3),
        nacos_zone_signal("third.local", || 2, || 0),
    ]);

    let signals = live.snapshot()["signals"].as_object().unwrap().clone();
    assert!(
        signals.get("nacos.old.local").is_none(),
        "the retired zone must be gone, got {:?}",
        signals.keys()
    );
    assert_eq!(signals["nacos.new.local"]["cachedServices"], 1);
    assert_eq!(signals["nacos.third.local"]["cachedServices"], 2);
    assert_eq!(signals["xdp"]["attached"], false);
}

#[test]
fn replace_nacos_zones_with_no_zones_leaves_only_the_other_signals() {
    let live = LiveState::new();
    live.register_nacos_zone("gone.local", || 1, || 0);
    live.replace_nacos_zones(vec![]);

    let signals = live.snapshot()["signals"].as_object().unwrap().clone();
    assert!(
        signals.keys().next().is_none(),
        "a chain with no nacos plugin reports no zone signal"
    );
}
