#![allow(clippy::disallowed_methods)]

use doggy_dns_control::live::LiveState;

/// The zone signal reports one thing: how many names this process subscribed.
/// Instance data lives in the Nacos client's cache, so there is no cache size
/// or push timestamp to report.
#[test]
fn nacos_zone_signal_reports_subscribed_services() {
    let mut live = LiveState::new();
    live.register_nacos_zone("nacos.local", || 7);

    let snapshot = live.snapshot();
    let signal = &snapshot["signals"]["nacos.nacos.local"];
    assert_eq!(signal["subscribedServices"], 7);
    assert!(
        signal.get("lastPushSecondsAgo").is_none(),
        "the zone signal carries no push freshness"
    );
    assert!(
        signal.get("cachedServices").is_none(),
        "the signal does not pretend to count a cache doggy-dns does not keep"
    );
}

#[test]
fn xdp_signal_reports_attach_state() {
    let mut live = LiveState::new();
    live.register_xdp(|| true);

    let snapshot = live.snapshot();
    assert_eq!(snapshot["signals"]["xdp"]["attached"], true);
}
