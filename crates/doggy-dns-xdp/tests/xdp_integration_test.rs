#![allow(clippy::disallowed_methods)]

// These tests require:
// - Linux with kernel >= 5.10
// - sudo / root privileges
// - veth pair support
//
// Run with: sudo -E cargo test -p doggy-dns-xdp --test xdp_integration_test -- --nocapture

#[cfg(target_os = "linux")]
mod linux_tests {
    use std::process::Command;

    fn setup_netns() {
        let _ = Command::new("ip").args(["netns", "add", "doggy-dns-test"]).output();
        let _ = Command::new("ip")
            .args(["link", "add", "veth0", "type", "veth", "peer", "name", "veth1"])
            .output();
        let _ = Command::new("ip")
            .args(["link", "set", "veth1", "netns", "doggy-dns-test"])
            .output();
        let _ = Command::new("ip")
            .args(["addr", "add", "10.0.0.1/24", "dev", "veth0"])
            .output();
        let _ = Command::new("ip").args(["link", "set", "veth0", "up"]).output();
        let _ = Command::new("ip")
            .args(["netns", "exec", "doggy-dns-test", "ip", "addr", "add", "10.0.0.2/24", "dev", "veth1"])
            .output();
        let _ = Command::new("ip")
            .args(["netns", "exec", "doggy-dns-test", "ip", "link", "set", "veth1", "up"])
            .output();
    }

    fn teardown_netns() {
        let _ = Command::new("ip").args(["netns", "delete", "doggy-dns-test"]).output();
        let _ = Command::new("ip").args(["link", "delete", "veth0"]).output();
    }

    #[test]
    #[ignore] // Requires root; run with: sudo -E cargo test -- --ignored --nocapture
    fn xdp_cache_hit_positive() {
        setup_netns();
        // TODO: Load XDP program on veth0, send DNS query, verify cache hit
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_cache_hit_negative_nxdomain() {
        setup_netns();
        // TODO: Send query for non-existent name, verify NXDomain cached
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_cache_ttl_expiry() {
        setup_netns();
        // TODO: Write entry with 1s TTL, wait 2s, verify miss
        teardown_netns();
    }

    #[test]
    #[ignore]
    fn xdp_non_dns_passthrough() {
        setup_netns();
        // TODO: Send packet to port 80, verify XDP_PASS
        teardown_netns();
    }
}
