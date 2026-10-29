#![allow(clippy::disallowed_methods)]

// These tests require:
// - Linux with kernel >= 5.10
// - sudo / root privileges
// - veth pair support
//
// Run with: sudo env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored --nocapture

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

    /// Only one XDP program can be bound to `lo` at a time, and ignored tests
    /// run in parallel threads by default: a second attach fails with EBUSY.
    /// These tests therefore serialise on this lock, and waiting for it is
    /// required — skipping instead would let CI pass without ever exercising
    /// the attached program.
    static LO_XDP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// An XDP manager attached to `lo`, holding the attach lock for as long as
    /// the manager lives.
    struct LoXdpGuard {
        manager: XdpManager,
        _shared: std::sync::MutexGuard<'static, ()>,
    }

    /// Block until this thread may attach to `lo`, then load and attach.
    fn attach_lo(config: &XdpConfig, port: u16) -> LoXdpGuard {
        // A panicking test poisons the lock but still releases it, and this
        // thread re-attaches from scratch, so recover the guard rather than
        // failing on the poison.
        let shared = LO_XDP_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match XdpManager::new(config, port) {
            Ok(manager) => LoXdpGuard {
                manager,
                _shared: shared,
            },
            Err(error) => panic!("load and attach XDP on port {port}: {error:#}"),
        }
    }

    #[test]
    #[ignore] // Requires root; run with: sudo env "PATH=$PATH" cargo test -- --ignored --nocapture
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

        let mut attach = attach_lo(&config, port);
        let mut cache = attach.manager.take_cache();
        assert_eq!(
            cache.max_entries().expect("read map info"),
            config.cache_size,
            "DNS_CACHE must use the configured capacity"
        );
        let stats = attach.manager.take_stats();
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

        // Loopback XDP answers within microseconds; poll briefly instead of
        // sleeping a fixed amount so a loaded CI box slows the run down rather
        // than failing it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let snapshot = loop {
            let snapshot = stats.snapshot().expect("read stats map");
            let complete = snapshot.hit >= 1
                && snapshot.negative_hit >= 1
                && snapshot.expired >= 1
                && snapshot.miss >= 1;
            if complete || tokio::time::Instant::now() >= deadline {
                break snapshot;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(snapshot.hit >= 1, "hit counter: {snapshot:?}");
        assert!(
            snapshot.negative_hit >= 1,
            "negative_hit counter: {snapshot:?}"
        );
        assert!(snapshot.expired >= 1, "expired counter: {snapshot:?}");
        assert!(snapshot.miss >= 1, "miss counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin_path);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_kernel_logs_reach_userspace() {
        // The pipeline proof anchors on a warn-level path that the default
        // level always forwards: a poisoned cache entry whose `resp_len`
        // exceeds the DNS bounds. The kernel logs the mismatch and passes the
        // query instead of answering it.
        use std::sync::atomic::Ordering;
        use tracing::Subscriber;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::util::SubscriberInitExt;

        /// Counts the forwarded poisoned-entry records. `TracingLog` routes
        /// every kernel record to the `doggy_dns_xdp::kernel` target and keeps
        /// the eBPF module path in the message body.
        #[derive(Default)]
        struct CountSubscriber {
            poison_warnings: std::sync::atomic::AtomicU32,
        }

        impl CountSubscriber {
            fn observe(&self, message: &str) {
                if message.contains("cached response length out of range: 600") {
                    self.poison_warnings.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        struct CountLayer(std::sync::Arc<CountSubscriber>);

        impl<S: Subscriber> Layer<S> for CountLayer {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                if event.metadata().target() != "doggy_dns_xdp::kernel" {
                    return;
                }
                struct MessageVisitor(std::sync::Arc<CountSubscriber>);
                impl tracing::field::Visit for MessageVisitor {
                    fn record_debug(
                        &mut self,
                        _field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.observe(&format!("{value:?}"));
                    }
                }
                event.record(&mut MessageVisitor(self.0.clone()));
            }
        }

        let port: u16 = 5534;
        let pin_path = format!("/sys/fs/bpf/doggy-dns-test/logs-{port}");
        let _ = std::fs::remove_file(&pin_path);

        let config = XdpConfig {
            enabled: true,
            interface: "lo".to_string(),
            cache_size: 1024,
            negative_ttl: 30,
            attach_mode: "auto".to_string(),
            pin_path: pin_path.clone(),
        };

        let mut attach = attach_lo(&config, port);

        // The drain task runs on this thread's current-thread runtime below,
        // and `TracingLog` records the kernel messages through whatever
        // subscriber is the default on this thread, so the counting subscriber
        // must stay installed until the assertion.
        let counter = std::sync::Arc::new(CountSubscriber::default());
        let _subscriber = tracing_subscriber::registry()
            .with(CountLayer(counter.clone()))
            .set_default();

        let mut cache = attach.manager.take_cache();
        let logger = attach
            .manager
            .take_log()
            .expect("AYA_LOGS map missing from the loaded program");

        // Drive the production drain path rather than calling `flush`
        // directly: the current-thread runtime only polls the drain task while
        // the test sleeps, so a forwarded record proves the AsyncFd wiring.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let enter = runtime.enter();
        let _drain = doggy_dns_xdp::program::spawn_log_flush(logger);

        let poison_name = "poison.example.";
        let (key, mut value) = compute_cache_entry(
            &Name::from_str(poison_name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(poison_name, ResponseCode::NoError),
            Some(60),
            30,
            now_ktime_ns(),
        );
        value.resp_len = 600;
        cache.insert(&key, &value).expect("insert poisoned entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok();
        socket
            .send_to(&query_bytes(poison_name), ("127.0.0.1", port))
            .expect("send poisoned query");

        // Poll until the drain task has forwarded the record: a fixed sleep
        // would either flake under load or slow every run down.
        let warnings = runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while counter.poison_warnings.load(Ordering::Relaxed) == 0 {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            counter.poison_warnings.load(Ordering::Relaxed)
        });
        assert!(
            warnings >= 1,
            "expected at least one kernel-side warn record for the poisoned cache entry, got {warnings}"
        );

        drop(cache);
        drop(enter);
        // The drain task polls forever by design; do not wait for it.
        runtime.shutdown_background();
        let _ = std::fs::remove_file(&pin_path);
    }
}
