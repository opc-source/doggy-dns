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
    use doggy_dns_xdp::stats::XdpStatsReader;
    use doggy_dns_xdp::types::XdpStats;
    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use std::net::UdpSocket;
    use std::str::FromStr;
    use std::time::Duration;

    /// Transaction id the helpers below stamp on cached responses; the query
    /// helper takes its own id so a test can prove the id gets rewritten.
    const CACHED_ID: u16 = 0x1234;

    fn query_for(name: &Name, id: u16) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        let mut query = Query::new();
        query.set_name(name.clone());
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize query")
    }

    fn query_bytes(name: &str) -> Vec<u8> {
        query_for(&Name::from_str(name).expect("valid name"), CACHED_ID)
    }

    fn response_for(name: &Name, code: ResponseCode) -> Vec<u8> {
        let mut message = Message::error_msg(CACHED_ID, OpCode::Query, code);
        let mut query = Query::new();
        query.set_name(name.clone());
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize response")
    }

    fn response_bytes(name: &str, code: ResponseCode) -> Vec<u8> {
        response_for(&Name::from_str(name).expect("valid name"), code)
    }

    /// Pin path for one test's maps. Unique per test so the ignored suite can
    /// run in any order without one test unlinking another's map.
    fn pin_path(tag: &str, port: u16) -> String {
        format!("/sys/fs/bpf/doggy-dns-test/{tag}-{port}")
    }

    fn config_for(pin_path: &str) -> XdpConfig {
        XdpConfig {
            enabled: true,
            interface: "lo".to_string(),
            cache_size: 1024,
            negative_ttl: 30,
            attach_mode: "auto".to_string(),
            pin_path: pin_path.to_string(),
        }
    }

    /// Receive one datagram, or `None` when nothing arrives before the
    /// timeout. XDP answers land on the sending socket within microseconds;
    /// a query the kernel passes on has no listener here and yields an ICMP
    /// port-unreachable error, which also reports as `None`.
    fn recv_datagram(socket: &UdpSocket) -> Option<Vec<u8>> {
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut buf = [0u8; 2048];
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => Some(buf[..len].to_vec()),
            Err(_) => None,
        }
    }

    /// Poll the per-CPU counters until `done` holds or two seconds pass, then
    /// return the last snapshot. The kernel counts on whichever CPU handled
    /// the softirq, which need not be this thread's.
    fn wait_for_stats(stats: &XdpStatsReader, done: impl Fn(&XdpStats) -> bool) -> XdpStats {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = stats.snapshot().expect("read stats map");
            if done(&snapshot) || tokio::time::Instant::now() >= deadline {
                return snapshot;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
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
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cache_hit_positive() {
        let port: u16 = 5531;
        let pin = pin_path("hit", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();

        let name = "positive.example.";
        let cached = response_bytes(name, ResponseCode::NoError);
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            cached.clone(),
            Some(60),
            30,
            now_ktime_ns(),
        );
        cache.insert(&key, &value).expect("insert positive entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        // A different id from the one the cached reply carries: the kernel
        // must patch it and leave every other byte alone.
        let query_id: u16 = 0x7777;
        socket
            .send_to(
                &query_for(&Name::from_str(name).expect("name"), query_id),
                ("127.0.0.1", port),
            )
            .expect("send query");

        let reply = recv_datagram(&socket).expect("cached reply not answered in time");
        let mut expected = cached;
        expected[..2].copy_from_slice(&query_id.to_be_bytes());
        assert_eq!(
            reply, expected,
            "cached reply must be copied verbatim with only the transaction id patched"
        );

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cache_hit_negative_nxdomain() {
        let port: u16 = 5532;
        let pin = pin_path("nx", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();

        let name = "nx.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NXDomain,
            response_bytes(name, ResponseCode::NXDomain),
            Some(60),
            30,
            now_ktime_ns(),
        );
        cache.insert(&key, &value).expect("insert negative entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let query_id: u16 = 0x8888;
        let query = query_for(&Name::from_str(name).expect("name"), query_id);
        socket
            .send_to(&query, ("127.0.0.1", port))
            .expect("send query");

        // The kernel does not copy the cached bytes for a negative hit: it
        // rewrites the query's header in place, so the answer is the query's
        // own id, QR=1, RCODE=3, empty answer/authority/additional sections,
        // and the question echoed verbatim.
        let reply = recv_datagram(&socket).expect("nxdomain reply not answered in time");
        let mut expected = query[..12].to_vec();
        expected[2] = 0x80 | (query[2] & 0x01); // set QR, keep RD
        expected[3] = 0x03;
        expected[6..12].fill(0);
        expected.extend_from_slice(&query[12..]);
        assert_eq!(
            reply, expected,
            "synthesised NXDOMAIN must echo the question and clear the other sections"
        );

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cache_ttl_expiry() {
        let port: u16 = 5536;
        let pin = pin_path("expired", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        let stats = attach.manager.take_stats();

        let name = "expired.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(name, ResponseCode::NoError),
            Some(1),
            1,
            now_ktime_ns().saturating_sub(2_000_000_000),
        );
        cache.insert(&key, &value).expect("insert expired entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        socket
            .send_to(&query_bytes(name), ("127.0.0.1", port))
            .expect("send query");

        // An expired entry is passed to userspace, and nothing listens on this
        // port in the test, so the query must go unanswered rather than being
        // served stale.
        assert!(
            recv_datagram(&socket).is_none(),
            "expired entry must not be answered from the cache"
        );
        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.expired >= 1);
        assert!(snapshot.expired >= 1, "expired counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_non_dns_passthrough() {
        let port: u16 = 5530;
        let other_port: u16 = 5539;
        let pin = pin_path("passthrough", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);
        let stats = attach.manager.take_stats();

        // XDP runs before the stack, so these sockets only ever see datagrams
        // the program passed on.
        let listener = UdpSocket::bind(("127.0.0.1", port)).expect("bind dns port");
        let other = UdpSocket::bind(("127.0.0.1", other_port)).expect("bind other port");
        let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender");

        // A query the cache does not hold is a miss: passed on untouched and
        // counted once.
        let miss = query_bytes("unseen.example.");
        sender
            .send_to(&miss, ("127.0.0.1", port))
            .expect("send miss");
        assert_eq!(
            recv_datagram(&listener).expect("missed query was not passed on"),
            miss
        );
        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.miss >= 1);
        assert_eq!(snapshot.miss, 1, "miss counter: {snapshot:?}");

        // Traffic to any other port never reaches the DNS parser.
        let elsewhere = query_bytes("elsewhere.example.");
        sender
            .send_to(&elsewhere, ("127.0.0.1", other_port))
            .expect("send to other port");
        assert_eq!(
            recv_datagram(&other).expect("non-DNS-port packet was not passed on"),
            elsewhere
        );
        let snapshot = stats.snapshot().expect("read stats map");
        assert_eq!(
            snapshot.hit + snapshot.negative_hit + snapshot.miss + snapshot.expired + snapshot.drop,
            1,
            "only the miss may be counted: {snapshot:?}"
        );

        drop(attach);
        let _ = std::fs::remove_file(&pin);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cached_reply_edge_lengths() {
        let port: u16 = 5535;
        let pin = pin_path("edges", port);
        let _ = std::fs::remove_file(&pin);

        /// The kernel copies a cached reply verbatim and patches only the
        /// transaction id, so the copy paths only care about the length; the
        /// bytes below need not parse as a DNS message.
        fn synthetic_reply(len: usize) -> Vec<u8> {
            let mut reply = vec![0u8; len];
            reply[..2].copy_from_slice(&CACHED_ID.to_be_bytes());
            reply[2] = 0x81; // QR=1, RD=1
            reply
        }

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        let stats = attach.manager.take_stats();

        let root = Name::root();
        let seventeen = Name::from_str("seventeen.example.").expect("name");
        let aligned = Name::from_str("aligned.example.").expect("name");
        // 1. A root query's DNS message is 17 bytes, so fewer than eight bytes
        //    follow the QNAME start: the kernel takes its byte-at-a-time
        //    fallback instead of the 8-byte block scan, and the reply is
        //    exactly MIN_POSITIVE_LEN (17), the shortest the copy admits.
        // 2. 32 bytes is a whole number of 16-byte chunks, so no lead chunk is
        //    written.
        let cases: [(&Name, Vec<u8>); 3] = [
            (&root, response_for(&root, ResponseCode::NoError)),
            (&seventeen, synthetic_reply(17)),
            (&aligned, synthetic_reply(32)),
        ];

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        for (index, (name, reply)) in cases.iter().enumerate() {
            let (key, value) = compute_cache_entry(
                name,
                1,
                1,
                ResponseCode::NoError,
                reply.clone(),
                Some(60),
                30,
                now_ktime_ns(),
            );
            cache.insert(&key, &value).expect("insert edge entry");

            let query_id = 0x9000 + index as u16;
            socket
                .send_to(&query_for(name, query_id), ("127.0.0.1", port))
                .expect("send query");
            let answered = recv_datagram(&socket).expect("edge-length reply not answered in time");

            let mut expected = reply.clone();
            expected[..2].copy_from_slice(&query_id.to_be_bytes());
            assert_eq!(
                answered,
                expected,
                "{name} ({} reply bytes) came back wrong",
                reply.len()
            );
        }

        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.hit >= 3);
        assert_eq!(snapshot.hit, 3, "hit counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo
    fn xdp_stats_reflect_cache_outcomes() {
        let port: u16 = 5533;
        let pin = pin_path("stats", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        assert_eq!(
            cache.max_entries().expect("read map info"),
            1024,
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
        let snapshot = wait_for_stats(&stats, |snapshot| {
            snapshot.hit >= 1
                && snapshot.negative_hit >= 1
                && snapshot.expired >= 1
                && snapshot.miss >= 1
        });
        assert!(snapshot.hit >= 1, "hit counter: {snapshot:?}");
        assert!(
            snapshot.negative_hit >= 1,
            "negative_hit counter: {snapshot:?}"
        );
        assert!(snapshot.expired >= 1, "expired counter: {snapshot:?}");
        assert!(snapshot.miss >= 1, "miss counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&pin);
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
        let pin = pin_path("logs", port);
        let _ = std::fs::remove_file(&pin);

        let mut attach = attach_lo(&config_for(&pin), port);

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
        let _drain = doggy_dns_xdp::kernel_log::spawn_log_flush(logger);

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
        let _ = std::fs::remove_file(&pin);
    }
}
