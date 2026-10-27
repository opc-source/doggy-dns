#![allow(clippy::disallowed_methods)]

// These tests require:
// - Linux with kernel >= 5.10
// - sudo / root privileges
// - veth pair support
//
// Run with: sudo -E cargo test -p doggy-dns-xdp --test xdp_integration_test -- --nocapture

#[cfg(target_os = "linux")]
mod linux_tests {
    use doggy_dns_basic::XdpConfig;
    use doggy_dns_xdp::XdpManager;
    use doggy_dns_xdp::cache::now_ktime_ns;
    use doggy_dns_xdp::middleware::compute_cache_entry;
    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use std::net::UdpSocket;
    use std::process::Command;
    use std::str::FromStr;
    use std::time::Duration;

    fn query_bytes(name: &str) -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        let mut query = Query::new();
        query.set_name(Name::from_str(name).expect("valid name"));
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize query")
    }

    fn response_bytes(name: &str, code: ResponseCode) -> Vec<u8> {
        let mut message = Message::error_msg(0x1234, OpCode::Query, code);
        let mut query = Query::new();
        query.set_name(Name::from_str(name).expect("valid name"));
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize response")
    }

    fn setup_netns() {
        let _ = Command::new("ip")
            .args(["netns", "add", "doggy-dns-test"])
            .output();
        let _ = Command::new("ip")
            .args([
                "link", "add", "veth0", "type", "veth", "peer", "name", "veth1",
            ])
            .output();
        let _ = Command::new("ip")
            .args(["link", "set", "veth1", "netns", "doggy-dns-test"])
            .output();
        let _ = Command::new("ip")
            .args(["addr", "add", "10.0.0.1/24", "dev", "veth0"])
            .output();
        let _ = Command::new("ip")
            .args(["link", "set", "veth0", "up"])
            .output();
        let _ = Command::new("ip")
            .args([
                "netns",
                "exec",
                "doggy-dns-test",
                "ip",
                "addr",
                "add",
                "10.0.0.2/24",
                "dev",
                "veth1",
            ])
            .output();
        let _ = Command::new("ip")
            .args([
                "netns",
                "exec",
                "doggy-dns-test",
                "ip",
                "link",
                "set",
                "veth1",
                "up",
            ])
            .output();
    }

    fn teardown_netns() {
        let _ = Command::new("ip")
            .args(["netns", "delete", "doggy-dns-test"])
            .output();
        let _ = Command::new("ip")
            .args(["link", "delete", "veth0"])
            .output();
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

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo
    fn xdp_stats_reflect_cache_outcomes() {
        let port: u16 = 5533;
        let pin_path = format!("/sys/fs/bpf/doggy-dns-test/stats-{port}");
        let _ = std::fs::remove_file(&pin_path);

        let config = XdpConfig {
            enabled: true,
            interface: "lo".to_string(),
            cache_size: 1024,
            negative_ttl: 30,
            attach_mode: "auto".to_string(),
            pin_path: pin_path.clone(),
        };

        let mut manager = XdpManager::new(&config, port).expect("load and attach XDP");
        let mut cache = manager.take_cache();
        let stats = manager.take_stats();
        let now = now_ktime_ns();

        let hit_name = "hit.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(hit_name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(hit_name, ResponseCode::NoError),
            Some(60),
            30,
            now,
        );
        cache.insert(&key, &value).expect("insert positive entry");

        let nx_name = "nx.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(nx_name).expect("name"),
            1,
            1,
            ResponseCode::NXDomain,
            response_bytes(nx_name, ResponseCode::NXDomain),
            Some(60),
            30,
            now,
        );
        cache.insert(&key, &value).expect("insert negative entry");

        let expired_name = "expired.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(expired_name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(expired_name, ResponseCode::NoError),
            Some(1),
            1,
            now.saturating_sub(2_000_000_000),
        );
        cache.insert(&key, &value).expect("insert expired entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok();

        for name in [hit_name, nx_name, expired_name, "miss.example."] {
            socket
                .send_to(&query_bytes(name), ("127.0.0.1", port))
                .expect("send query");
        }

        std::thread::sleep(Duration::from_millis(200));

        let snapshot = stats.snapshot().expect("read stats map");
        assert!(snapshot.hit >= 1, "hit counter: {snapshot:?}");
        assert!(
            snapshot.negative_hit >= 1,
            "negative_hit counter: {snapshot:?}"
        );
        assert!(snapshot.expired >= 1, "expired counter: {snapshot:?}");
        assert!(snapshot.miss >= 1, "miss counter: {snapshot:?}");

        drop(manager);
        let _ = std::fs::remove_file(&pin_path);
    }
}
