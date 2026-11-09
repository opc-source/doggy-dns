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
    use doggy_dns_xdp::cache::make_cache_key;
    use doggy_dns_xdp::cache::now_ktime_ns;
    use doggy_dns_xdp::middleware::compute_cache_entry;
    use doggy_dns_xdp::pin::versioned_pin_path;
    use doggy_dns_xdp::stats::XdpStatsReader;
    use doggy_dns_xdp::types::XdpStats;
    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};
    use std::net::{Ipv4Addr, UdpSocket};
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

    /// The bpffs file `XdpManager` actually creates for the config path above:
    /// the loader appends the cache layout version, so every cleanup here has
    /// to name that same file or the next run inherits this run's map.
    fn pinned_file(config_pin_path: &str) -> String {
        versioned_pin_path(config_pin_path)
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
        let _ = std::fs::remove_file(pinned_file(&pin));

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();

        let name = "positive.example.";
        let cached = response_bytes(name, ResponseCode::NoError);
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            &cached,
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
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cache_hit_negative_nxdomain() {
        let port: u16 = 5532;
        let pin = pin_path("nx", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();

        let name = "nx.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NXDomain,
            &response_bytes(name, ResponseCode::NXDomain),
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
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cache_ttl_expiry() {
        let port: u16 = 5536;
        let pin = pin_path("expired", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        let stats = attach.manager.take_stats();

        let name = "expired.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            &response_bytes(name, ResponseCode::NoError),
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
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_non_dns_passthrough() {
        let port: u16 = 5530;
        let other_port: u16 = 5539;
        let pin = pin_path("passthrough", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

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
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_cached_reply_edge_lengths() {
        let port: u16 = 5535;
        let pin = pin_path("edges", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

        /// The kernel copies the cached header words and the tail after the
        /// question, keeping the query's transaction id and question bytes,
        /// so the tail bytes below need not parse as DNS records; only the
        /// length matters to the copy paths.
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
        //    written. Both synthetic replies end before their question would,
        //    exercising the no-tail path: only the header words are copied.
        let mut cases: Vec<(&Name, Vec<u8>)> = vec![
            (&root, response_for(&root, ResponseCode::NoError)),
            (&seventeen, synthetic_reply(17)),
            (&aligned, synthetic_reply(32)),
        ];
        // 3. Tails of every copy shape after a real question: shorter than a
        //    chunk (byte loop), exactly one chunk, and chunk-plus-lead with
        //    and without alignment.
        let tail_name = Name::from_str("tail.example.").expect("name");
        let question_only = response_for(&tail_name, ResponseCode::NoError);
        for tail_len in [1usize, 15, 16, 17, 31, 32] {
            let mut reply = question_only.clone();
            reply.extend_from_slice(&vec![0xA5; tail_len]);
            cases.push((&tail_name, reply));
        }

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        for (index, (name, reply)) in cases.iter().enumerate() {
            let (key, value) = compute_cache_entry(
                name,
                1,
                1,
                ResponseCode::NoError,
                &reply,
                Some(60),
                30,
                now_ktime_ns(),
            );
            cache.insert(&key, &value).expect("insert edge entry");

            let query_id = 0x9000 + index as u16;
            let query = query_for(name, query_id);
            socket
                .send_to(&query, ("127.0.0.1", port))
                .expect("send query");
            let answered = recv_datagram(&socket).expect("edge-length reply not answered in time");

            // The replay keeps the query's id and question bytes; everything
            // else comes from the cached reply. A reply shorter than the
            // question only carries the question's leading bytes.
            let mut expected = reply.clone();
            expected[..2].copy_from_slice(&query_id.to_be_bytes());
            let question_end = query.len().min(reply.len());
            expected[12..question_end].copy_from_slice(&query[12..question_end]);
            assert_eq!(
                answered,
                expected,
                "{name} ({} reply bytes) came back wrong",
                reply.len()
            );
        }

        let expected_hits = cases.len() as u64;
        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.hit >= expected_hits);
        assert_eq!(snapshot.hit, expected_hits, "hit counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo
    fn xdp_stats_reflect_cache_outcomes() {
        let port: u16 = 5533;
        let pin = pin_path("stats", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

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
            &response_bytes(hit_name, ResponseCode::NoError),
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
            &response_bytes(nx_name, ResponseCode::NXDomain),
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
            &response_bytes(expired_name, ResponseCode::NoError),
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
        let _ = std::fs::remove_file(pinned_file(&pin));
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
        let _ = std::fs::remove_file(pinned_file(&pin));

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
            &response_bytes(poison_name, ResponseCode::NoError),
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
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_pinned_cache_survives_a_restart() {
        let port: u16 = 5541;
        let pin = pin_path("reuse", port);
        let on_disk = pinned_file(&pin);
        let _ = std::fs::remove_file(&on_disk);

        let name = "survivor.example.";
        let cached = response_bytes(name, ResponseCode::NoError);

        // First generation: the loader creates and pins the map, one entry is
        // written, then the manager goes away — which detaches the program but
        // leaves the pinned map and its entry in bpffs.
        {
            let mut attach = attach_lo(&config_for(&pin), port);
            let mut cache = attach.manager.take_cache();
            let (key, value) = compute_cache_entry(
                &Name::from_str(name).expect("name"),
                1,
                1,
                ResponseCode::NoError,
                &cached,
                Some(600),
                30,
                now_ktime_ns(),
            );
            cache.insert(&key, &value).expect("insert survivor entry");
            drop(cache);
        }

        // Second generation on the same path, asking for a different capacity.
        // Reusing the pinned map is the only way this can come up: creating a
        // map at an existing pin path fails with `File exists`.
        let second = XdpConfig {
            cache_size: 2048,
            ..config_for(&pin)
        };
        let mut attach = attach_lo(&second, port);
        let mut cache = attach.manager.take_cache();
        assert_eq!(
            cache.max_entries().expect("read map info"),
            1024,
            "the second start must reuse the pinned map, whose capacity comes \
             from the first config, not the 2048 the second one asks for"
        );
        let survivor_key = make_cache_key(&Name::from_str(name).expect("name"), 1, 1);
        assert!(
            cache
                .get(&survivor_key)
                .expect("lookup surviving entry")
                .is_some(),
            "the entry written before the restart must still be in the map"
        );
        let stats = attach.manager.take_stats();

        // The newly attached program answers from that entry without ever
        // having written it, so the kernel side holds the same map.
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let query_id: u16 = 0x6666;
        socket
            .send_to(
                &query_for(&Name::from_str(name).expect("name"), query_id),
                ("127.0.0.1", port),
            )
            .expect("send query");
        let reply = recv_datagram(&socket).expect("surviving entry not answered in time");
        let mut expected = cached;
        expected[..2].copy_from_slice(&query_id.to_be_bytes());
        assert_eq!(
            reply, expected,
            "the restarted program must replay the entry its predecessor wrote"
        );

        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.hit >= 1);
        assert!(snapshot.hit >= 1, "hit counter: {snapshot:?}");
        assert_eq!(
            snapshot.miss, 0,
            "a freshly created map would have missed this query: {snapshot:?}"
        );

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&on_disk);
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn xdp_reuse_warns_when_the_pinned_map_does_not_fit() {
        use tracing::Subscriber;
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::util::SubscriberInitExt;

        /// Collects the text of every `doggy_dns_xdp::program` record so the
        /// assertion can name the mismatch the loader is expected to report.
        #[derive(Default)]
        struct ProseSubscriber {
            records: std::sync::Mutex<Vec<String>>,
        }

        impl ProseSubscriber {
            fn observe(&self, text: &str) {
                self.records
                    .lock()
                    .expect("records lock")
                    .push(text.to_string());
            }

            fn joined(&self) -> String {
                self.records.lock().expect("records lock").join("\n")
            }
        }

        struct CollectLayer(std::sync::Arc<ProseSubscriber>);

        impl<S: Subscriber> Layer<S> for CollectLayer {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                if event.metadata().target() != "doggy_dns_xdp::program" {
                    return;
                }
                struct TextField(std::sync::Arc<ProseSubscriber>);
                impl tracing::field::Visit for TextField {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        // `message` is the event's own text; `details` and
                        // friends are the named fields, and a capacity
                        // mismatch arrives as `details`.
                        self.0.observe(&format!("{}={:?}", field.name(), value));
                    }
                }
                event.record(&mut TextField(self.0.clone()));
            }
        }

        let port: u16 = 5542;
        let pin = pin_path("reuse-warn", port);
        let on_disk = pinned_file(&pin);
        let _ = std::fs::remove_file(&on_disk);

        // First generation pins a map of 1024 entries.
        {
            let attach = attach_lo(&config_for(&pin), port);
            drop(attach);
        }

        // Second generation asks for 2048. The pinned map cannot be resized, so
        // the operator has to be told the configured value did not take effect.
        let second = XdpConfig {
            cache_size: 2048,
            ..config_for(&pin)
        };
        let subscriber = std::sync::Arc::new(ProseSubscriber::default());
        let _default = tracing_subscriber::registry()
            .with(CollectLayer(subscriber.clone()))
            .set_default();
        let mut attach = attach_lo(&second, port);
        let cache = attach.manager.take_cache();
        assert_eq!(
            cache.max_entries().expect("read map info"),
            1024,
            "the pinned map's capacity wins"
        );
        let records = subscriber.joined();
        assert!(
            records.contains("capacity 1024 != configured 2048"),
            "expected a warning naming the capacity mismatch, got: {records}"
        );

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(&on_disk);
    }

    /// A positive reply for `name` whose answer section holds one A record,
    /// so a replay has bytes beyond the question to copy. hickory emits the
    /// question in canonical lowercase; a caller wanting a specific 0x20 case
    /// stamps it onto the QNAME wire bytes afterwards.
    fn positive_response_with_answer(name: &Name) -> Vec<u8> {
        let mut message = Message::new(CACHED_ID, MessageType::Response, OpCode::Query);
        let mut query = Query::new();
        query.set_name(name.clone());
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.add_answer(Record::from_rdata(
            name.clone(),
            60,
            RData::A(A(Ipv4Addr::new(203, 0, 113, 9))),
        ));
        message.to_vec().expect("serialize response")
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn positive_hit_preserves_query_case() {
        let port: u16 = 5543;
        let pin = pin_path("case", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        let stats = attach.manager.take_stats();

        // The cache writer's question uses one case of the name. hickory
        // emits names in canonical (lowercase) form, so the exact 0x20 case
        // is stamped onto the question's QNAME wire bytes after serialising;
        // the key hashes lowercased bytes and is unaffected.
        let writer_name = Name::from_str("test.example.").expect("name");
        let mut cached = positive_response_with_answer(&writer_name);
        let writer_case = b"\x04TeSt\x07ExAmPlE\x00";
        cached[12..12 + writer_case.len()].copy_from_slice(writer_case);
        let (key, value) = compute_cache_entry(
            &writer_name,
            1,
            1,
            ResponseCode::NoError,
            &cached,
            Some(60),
            30,
            now_ktime_ns(),
        );
        cache.insert(&key, &value).expect("insert positive entry");

        // ...and the querying client (DNS 0x20) uses another case of the same
        // name. The key hashes lowercased wire bytes, so this is a cache hit.
        let query_id: u16 = 0x5A5A;
        let mut query = query_for(&writer_name, query_id);
        let query_case = b"\x04tEsT\x07eXaMpLe\x00";
        query[12..12 + query_case.len()].copy_from_slice(query_case);
        // The query carries only a question section, so everything after the
        // header is the question, and hash equality means the cached reply's
        // question has exactly the same wire length.
        let question_len = query.len() - 12;
        assert_ne!(
            &cached[12..12 + question_len],
            &query[12..],
            "test setup: the two cases must actually differ"
        );

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        socket
            .send_to(&query, ("127.0.0.1", port))
            .expect("send query");
        let reply = recv_datagram(&socket).expect("cached reply not answered in time");

        assert_eq!(
            &reply[..2],
            &query_id.to_be_bytes(),
            "transaction id must be the query's"
        );
        assert_eq!(
            &reply[12..12 + question_len],
            &query[12..],
            "question must echo the query's own case: got {:?}, want {:?}",
            String::from_utf8_lossy(&reply[12..12 + question_len]),
            String::from_utf8_lossy(&query[12..])
        );
        let mut expected = cached;
        expected[..2].copy_from_slice(&query_id.to_be_bytes());
        expected[12..12 + question_len].copy_from_slice(&query[12..]);
        assert_eq!(
            reply, expected,
            "header words and answer records must still match the cached reply"
        );

        // Exactly one query was sent on this test's private stats map.
        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.hit >= 1);
        assert_eq!(snapshot.hit, 1, "hit counter: {snapshot:?}");

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(pinned_file(&pin));
    }

    /// One's-complement checksum over an IPv4 header whose check field is zero.
    fn ipv4_header_checksum(header: &[u8]) -> u16 {
        let mut sum = 0u32;
        for pair in header.chunks_exact(2) {
            sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// Hand-build an Ethernet + IPv4 + UDP frame carrying `payload`, with the
    /// IPv4 fragment field set verbatim. The UDP checksum stays zero (optional
    /// for IPv4); the IP header checksum is computed so the stack accepts the
    /// frame when XDP passes it on.
    fn fragmented_udp_frame(
        src_port: u16,
        dst_port: u16,
        ip_id: u16,
        frag_off: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let mut frame = vec![0u8; 14 + 20 + udp_len];
        // Ethernet: loopback ignores the addresses; only the type matters.
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = &mut frame[14..34];
        ip[0] = 0x45; // IPv4, 20-byte header
        ip[2..4].copy_from_slice(&((20 + udp_len) as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&ip_id.to_be_bytes());
        ip[6..8].copy_from_slice(&frag_off.to_be_bytes());
        ip[8] = 64; // TTL
        ip[9] = 17; // UDP
        ip[12..16].copy_from_slice(&[127, 0, 0, 1]);
        ip[16..20].copy_from_slice(&[127, 0, 0, 1]);
        let check = ipv4_header_checksum(ip);
        ip[10..12].copy_from_slice(&check.to_be_bytes());
        let udp = &mut frame[34..];
        udp[0..2].copy_from_slice(&src_port.to_be_bytes());
        udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
        udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        udp[8..].copy_from_slice(payload);
        frame
    }

    /// Inject one raw Ethernet frame on `lo` via AF_PACKET, so the IPv4
    /// fragment field reaches XDP exactly as built (a UDP socket cannot set
    /// it). The frame takes the same loopback receive path as ordinary
    /// traffic, which is where the XDP program runs.
    fn send_frame_on_lo(frame: &[u8]) {
        // Safety: a zeroed sockaddr_ll is a valid initialised value, and the
        // pointers below stay live for the duration of each call.
        unsafe {
            let fd = libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                (libc::ETH_P_IP as u16).to_be() as i32,
            );
            assert!(
                fd >= 0,
                "open AF_PACKET socket: {}",
                std::io::Error::last_os_error()
            );
            let ifindex = libc::if_nametoindex(b"lo\0".as_ptr() as *const libc::c_char);
            assert!(
                ifindex != 0,
                "look up lo: {}",
                std::io::Error::last_os_error()
            );
            let mut addr: libc::sockaddr_ll = std::mem::zeroed();
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = (libc::ETH_P_IP as u16).to_be();
            addr.sll_ifindex = ifindex as i32;
            addr.sll_halen = 6;
            let sent = libc::sendto(
                fd,
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
                &addr as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            );
            libc::close(fd);
            assert_eq!(
                sent,
                frame.len() as isize,
                "inject frame: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    /// Open a non-blocking AF_PACKET tap on `lo` that sees every frame the
    /// interface receives (injected queries and XDP_TX replies alike). It is
    /// needed because loopback does not deliver an XDP_TX reply to an
    /// AF_PACKET-injected query back to a UDP socket: the reply is on the wire
    /// (and counted as a hit) but never reaches `recv_from`. Open the tap
    /// before sending the query; the caller closes the fd with `libc::close`.
    fn open_lo_sniffer() -> libc::c_int {
        // Safety: a zeroed sockaddr_ll is a valid initialised value, and the
        // pointers below stay live for the duration of each call.
        unsafe {
            let fd = libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                (libc::ETH_P_ALL as u16).to_be() as i32,
            );
            assert!(
                fd >= 0,
                "open lo sniffer: {}",
                std::io::Error::last_os_error()
            );
            let ifindex = libc::if_nametoindex(b"lo\0".as_ptr() as *const libc::c_char);
            assert!(
                ifindex != 0,
                "look up lo: {}",
                std::io::Error::last_os_error()
            );
            let mut addr: libc::sockaddr_ll = std::mem::zeroed();
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
            addr.sll_ifindex = ifindex as i32;
            let bound = libc::bind(
                fd,
                &addr as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            );
            assert_eq!(
                bound,
                0,
                "bind lo sniffer: {}",
                std::io::Error::last_os_error()
            );
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            assert!(
                flags >= 0,
                "read sniffer flags: {}",
                std::io::Error::last_os_error()
            );
            assert_eq!(
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0,
                "set sniffer nonblock: {}",
                std::io::Error::last_os_error()
            );
            fd
        }
    }

    /// Receive one tapped frame, or `None` when none is queued right now.
    fn recv_tapped_frame(fd: libc::c_int) -> Option<Vec<u8>> {
        let mut buf = [0u8; 2048];
        // Safety: `buf` is live for the call and `fd` is a valid open socket.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n > 0 {
            Some(buf[..n as usize].to_vec())
        } else {
            None
        }
    }

    /// The DNS payload of a tapped Ethernet + IPv4 + UDP frame, if it is one.
    fn tapped_dns_payload(frame: &[u8]) -> Option<&[u8]> {
        if frame.len() < 14 + 20 + 8 {
            return None;
        }
        if u16::from_be_bytes([frame[12], frame[13]]) != 0x0800 {
            return None;
        }
        let ip = &frame[14..];
        if ip[9] != 17 {
            return None;
        }
        let ip_hdr_len = (ip[0] & 0x0f) as usize * 4;
        if ip.len() < ip_hdr_len + 8 {
            return None;
        }
        Some(&ip[ip_hdr_len + 8..])
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo.
    fn fragmented_query_passes() {
        let port: u16 = 5544;
        let pin = pin_path("frag", port);
        let _ = std::fs::remove_file(pinned_file(&pin));

        let mut attach = attach_lo(&config_for(&pin), port);
        let mut cache = attach.manager.take_cache();
        let stats = attach.manager.take_stats();

        // Hold a cacheable answer for the payload's question, so a program
        // that parses past the fragment check would answer (and count a hit)
        // instead of passing.
        let name = "fragmented.example.";
        let cached = response_bytes(name, ResponseCode::NoError);
        let (key, value) = compute_cache_entry(
            &Name::from_str(name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            &cached,
            Some(60),
            30,
            now_ktime_ns(),
        );
        cache.insert(&key, &value).expect("insert positive entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let src_port = socket.local_addr().expect("sender addr").port();
        let payload = query_bytes(name);

        // A first fragment (MF set, offset 0) and a non-first fragment
        // (offset 8 bytes). Distinct IP ids keep the kernel's reassembly
        // queues separate so the frames can never complete a datagram.
        for (ip_id, frag_off) in [(0x0F01u16, 0x2000u16), (0x0F02, 0x0001)] {
            let frame = fragmented_udp_frame(src_port, port, ip_id, frag_off, &payload);
            send_frame_on_lo(&frame);
            assert!(
                recv_datagram(&socket).is_none(),
                "fragment field {frag_off:#06x} must not be answered with XDP_TX"
            );
        }

        // Positive control: DF alone (no MF, offset 0) is a complete,
        // unfragmented datagram and must still be answered. The fragment
        // mask is 0x3FFF (MF + offset); an over-broad 0x7FFF would also
        // catch DF and pass this frame, leaving the hit counter at zero.
        // Loopback never delivers the XDP_TX reply to an injected query back
        // to a UDP socket, so tap the wire for it instead of `recv_datagram`:
        // the reply is a response (QR=1), the injected query a query (QR=0).
        let sniffer = open_lo_sniffer();
        let frame = fragmented_udp_frame(src_port, port, 0x0F03, 0x4000, &payload);
        send_frame_on_lo(&frame);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let reply = loop {
            if let Some(tapped) = recv_tapped_frame(sniffer) {
                if let Some(dns) = tapped_dns_payload(&tapped) {
                    if dns.len() >= 12 && dns[2] & 0x80 != 0 {
                        break Some(dns.to_vec());
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Safety: `sniffer` is a valid open fd returned by `open_lo_sniffer`.
        unsafe { libc::close(sniffer) };
        let reply = reply.expect("DF-set query must produce an XDP_TX reply on the wire");
        // The query and the cached reply share the helpers' txid and canonical
        // question case, so the replayed answer equals the cached bytes.
        assert_eq!(reply, cached, "DF-set query must get the cached reply");

        // The receive timeouts above give the kernel ample time to run XDP
        // and update the per-CPU counters: the two fragments leave every
        // counter untouched, and only the DF-set query counts one hit.
        let snapshot = wait_for_stats(&stats, |snapshot| snapshot.hit >= 1);
        assert_eq!(snapshot.hit, 1, "hit counter: {snapshot:?}");
        assert_eq!(
            snapshot.negative_hit + snapshot.miss + snapshot.expired + snapshot.drop,
            0,
            "fragments must pass without touching the other cache counters: {snapshot:?}"
        );

        drop(cache);
        drop(attach);
        let _ = std::fs::remove_file(pinned_file(&pin));
    }
}
