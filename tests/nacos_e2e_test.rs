#![allow(clippy::disallowed_methods)]

//! End-to-end tests for the live Nacos path: `NacosServiceWatcher` -> the Nacos
//! client's naming cache -> `NacosAuthority` -> DNS answer.
//!
//! Every case needs a running Nacos server, so they are `#[ignore]`d and never
//! join `cargo test --workspace`. CI runs them in `.github/workflows/nacos-ci.yml`.
//!
//! Locally (Podman; the image comes from a mirror because Docker Hub is not
//! reachable from this network):
//!
//! ```text
//! podman run -d --name doggy-nacos \
//!   -e MODE=standalone -e JVM_XMS=384m -e JVM_XMX=512m \
//!   -p 8848:8848 -p 9848:9848 \
//!   docker.m.daocloud.io/nacos/nacos-server:v2.5.4
//! cargo test --test nacos_e2e_test -- --ignored --nocapture
//! ```
//!
//! `DOGGY_NACOS_ADDR` overrides the server address (default `127.0.0.1:8848`).
//!
//! Instances are registered with the Nacos SDK over gRPC and stay owned by this
//! test process: ephemeral instances disappear once the client connection is
//! dropped, so the `NacosClient` handle must outlive the queries below.

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use nacos_sdk::api::naming::{NamingService, NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::props::ClientProps;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};

const CONFIG_PATH: &str = "config/doggy-dns-nacos-e2e.toml";
const ZONE: &str = "nacos.e2e.test";
const GROUP: &str = "DEFAULT_GROUP";
const NAMESPACE: &str = "public";
const RECORD_TTL: u32 = 6;

/// Nacos answers and pushes are network round trips; the first subscribe after
/// a cold server start can take a few seconds, so wait generously rather than
/// turning CI jitter into a failure.
const DEADLINE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(300);

/// Longer than the watcher's three-second Nacos bound: a SERVFAIL for an
/// unreachable backend arrives just after that bound, so the client must not
/// give up at the same instant.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

fn nacos_addr() -> String {
    std::env::var("DOGGY_NACOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8848".to_string())
}

/// A Nacos naming client owned by the test; dropping it unregisters the
/// ephemeral instances registered through it.
struct NacosClient {
    naming: NamingService,
}

impl NacosClient {
    async fn connect() -> Self {
        let props = ClientProps::new()
            .server_addr(nacos_addr())
            .namespace(NAMESPACE)
            .app_name("doggy-dns-e2e");
        let naming = NamingServiceBuilder::new(props)
            .build()
            .await
            .expect("failed to build a Nacos naming client; is the server running?");
        Self { naming }
    }

    /// Register the full instance list of `service`.
    ///
    /// Nacos treats a batch registration as a replacement, so every call must
    /// carry all instances the service should have; they live only as long as
    /// this client's connection does (ephemeral).
    async fn register(&self, service: &str, instances: &[(&str, i32)]) {
        let instances = instances
            .iter()
            .map(|(ip, port)| test_instance(ip, *port))
            .collect();
        self.naming
            .batch_register_instance(service.to_string(), Some(GROUP.to_string()), instances)
            .await
            .expect("failed to register instances with Nacos");
    }

    /// Remove one ephemeral instance; the last removal empties the service.
    async fn deregister(&self, service: &str, ip: &str, port: i32) {
        self.naming
            .deregister_instance(
                service.to_string(),
                Some(GROUP.to_string()),
                test_instance(ip, port),
            )
            .await
            .expect("failed to deregister an instance with Nacos");
    }

    /// The server's own list for `service`, with no client cache in the way.
    ///
    /// `subscribe = false` reads through to the server, so this says what the
    /// registry actually holds rather than what some client last remembered.
    async fn server_ips(&self, service: &str) -> HashSet<String> {
        self.naming
            .get_all_instances(service.to_string(), Some(GROUP.to_string()), vec![], false)
            .await
            .unwrap_or_default()
            .iter()
            .map(|instance| instance.ip.clone())
            .collect()
    }

    /// Wait until the server's list for `service` is exactly `ips`.
    ///
    /// A registration is not queryable the instant the call returns; a case that
    /// asserts what a *later* process must read from the server has to wait for
    /// that state, or it measures the propagation instead of the behaviour.
    async fn await_server_has(&self, service: &str, ips: &[&str]) {
        let want: HashSet<String> = ips.iter().map(|ip| ip.to_string()).collect();
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let seen = self.server_ips(service).await;
            if seen == want {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the server never served {service} as {want:?}; it says {seen:?}");
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

fn test_instance(ip: &str, port: i32) -> ServiceInstance {
    ServiceInstance {
        ip: ip.to_string(),
        port,
        healthy: true,
        enabled: true,
        ephemeral: true,
        ..Default::default()
    }
}

/// A `doggy-dns` process running the e2e configuration on `port`.
///
/// Instances are registered before this is constructed so a case never depends
/// on the first query's subscription racing the server's start-up.
struct DoggyDns {
    port: u16,
    child: Child,
}

impl DoggyDns {
    /// Copy the committed e2e configuration with its `port` line set to `port`.
    ///
    /// `${ENV_VAR}` expansion only reaches string values (see
    /// `crates/doggy-dns-basic/src/env.rs`), so the numeric port cannot come
    /// from the environment and is patched here instead.
    fn case_config(port: u16) -> PathBuf {
        let raw = std::fs::read_to_string(CONFIG_PATH).expect("failed to read the e2e config");
        let patched: Vec<String> = raw
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("port = ") {
                    format!("port = {port}")
                } else {
                    line.to_string()
                }
            })
            .collect();

        let path =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nacos-e2e-{port}.toml"));
        std::fs::write(&path, patched.join("\n") + "\n").expect("failed to write the case config");
        path
    }

    async fn start(port: u16) -> Self {
        Self::start_with(port, &nacos_addr(), &Self::cache_dir(port)).await
    }

    async fn start_with_addr(port: u16, server_addr: &str) -> Self {
        Self::start_with(port, server_addr, &Self::cache_dir(port)).await
    }

    /// The Nacos client's naming cache directory for one case.
    ///
    /// `NACOS_CLIENT_CACHE_DIR` is read by the SDK (`env_first` is on by
    /// default), so pointing it under the test's temp dir keeps every case out of
    /// `~/nacos` — no run inherits another's cache, and a test cannot leave
    /// entries behind that change what a later start-up loads.
    fn cache_dir(port: u16) -> PathBuf {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nacos-cache-{port}"))
    }

    /// Start on `port` against `server_addr`, with the SDK's naming cache in
    /// `cache_dir`. Two processes sharing one `cache_dir` is how a case hands the
    /// first run's loaded cache to the second.
    async fn start_with(port: u16, server_addr: &str, cache_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_doggy-dns"))
            .arg(Self::case_config(port))
            .env("DOGGY_NACOS_ADDR", server_addr)
            .env("NACOS_CLIENT_CACHE_DIR", cache_dir)
            // Leave the output attached: `--nocapture` shows it in CI, and the
            // harness captures and replays it for a failing test.
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn doggy-dns");

        // A rejected configuration exits immediately; fail here instead of
        // polling a dead listener until the deadline.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(status) = child.try_wait().expect("failed to poll doggy-dns") {
            panic!("doggy-dns exited with {status} right after start-up");
        }

        Self { port, child }
    }

    /// Wait until the SDK has written `service`'s naming cache entry to disk.
    ///
    /// The client persists a cache entry when a push lands, not when a query
    /// reads it, so a case that needs the entry to survive the process has to
    /// watch for the file rather than assume the answer it just got was stored.
    async fn await_cached_on_disk(cache_dir: &Path, service: &str) -> PathBuf {
        // <cache_dir>/naming/<namespace>/<group>@@<service>
        let dir = cache_dir.join("naming").join(NAMESPACE);
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().ends_with(&format!("@@{service}")) {
                        return entry.path();
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the nacos client never wrote a disk cache entry for '{service}'");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Send one A query, returning `None` while the server is not answering yet.
    async fn send(&self, qname: &str, rtype: RecordType) -> Option<Message> {
        let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("failed to bind the DNS client socket");
        socket
            .connect((Ipv4Addr::LOCALHOST, self.port))
            .await
            .ok()?;

        let query_name = Name::from_ascii(qname).expect("failed to build the query name");
        let mut message = Message::new(0xead0, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(query_name, rtype));
        let bytes = message.to_vec().expect("failed to encode the DNS query");
        socket.send(&bytes).await.ok()?;

        let mut buffer = [0u8; 2048];
        let reply = tokio::time::timeout(REPLY_TIMEOUT, socket.recv(&mut buffer))
            .await
            .ok()?
            .ok()?;
        Message::from_vec(&buffer[..reply]).ok()
    }

    /// Poll until `predicate` accepts a response, and report what arrived.
    async fn poll_until(
        &self,
        qname: &str,
        rtype: RecordType,
        what: &str,
        predicate: impl Fn(&Message) -> bool,
    ) -> Message {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            if let Some(message) = self.send(qname, rtype).await
                && predicate(&message)
            {
                return message;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out after {DEADLINE:?} waiting for {what} on {qname}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait for exactly `want` answer records of `rtype`.
    async fn await_answers(&self, qname: &str, rtype: RecordType, want: usize) -> Message {
        self.poll_until(qname, rtype, &format!("{want} answers"), |message| {
            message.answers.len() == want && message.metadata.response_code == ResponseCode::NoError
        })
        .await
    }

    /// Wait for a specific response code.
    async fn await_code(&self, qname: &str, rtype: RecordType, code: ResponseCode) -> Message {
        self.poll_until(
            qname,
            rtype,
            &format!("response code {code:?}"),
            |message| message.metadata.response_code == code,
        )
        .await
    }
}

impl Drop for DoggyDns {
    fn drop(&mut self) {
        // `kill_on_drop` also covers the panic path; signalling here makes the
        // orderly shutdown explicit so a leaked process cannot hold the port.
        let _ = self.child.start_kill();
    }
}

/// `<service>.<group>.<namespace>.<zone>.` — the format `parse_dns_name` reads.
fn service_qname(service: &str) -> String {
    format!("{service}.{GROUP}.{NAMESPACE}.{ZONE}.")
}

fn answer_ips(message: &Message) -> HashSet<String> {
    message
        .answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::A(address) => Some(address.0.to_string()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn e2e_boot_time_instances_resolve() {
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-basic", &[("10.0.0.11", 8080), ("10.0.0.12", 8080)])
        .await;

    let server = DoggyDns::start(15411).await;
    let qname = service_qname("e2e-basic");
    let message = server.await_answers(&qname, RecordType::A, 2).await;

    assert_eq!(
        answer_ips(&message),
        HashSet::from(["10.0.0.11".to_string(), "10.0.0.12".to_string()]),
        "both instances registered before start-up must be served"
    );
    for record in &message.answers {
        assert_eq!(
            record.ttl, RECORD_TTL,
            "answers must carry the configured nacos plugin ttl"
        );
    }
}

#[tokio::test]
#[ignore]
async fn e2e_push_after_boot_adds_record() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-push", &[("10.0.0.21", 8080)]).await;

    let server = DoggyDns::start(15412).await;
    let qname = service_qname("e2e-push");
    server.await_answers(&qname, RecordType::A, 1).await;

    // The point of running a real server: a change pushed over the subscription
    // must replace the cached instance list, not wait for a restart.
    nacos
        .register("e2e-push", &[("10.0.0.21", 8080), ("10.0.0.22", 8080)])
        .await;

    let message = server.await_answers(&qname, RecordType::A, 2).await;
    assert_eq!(
        answer_ips(&message),
        HashSet::from(["10.0.0.21".to_string(), "10.0.0.22".to_string()]),
        "the pushed instance must appear without a restart"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_unknown_service_in_zone_is_nxdomain() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-known", &[("10.0.0.31", 8080)]).await;

    let server = DoggyDns::start(15413).await;
    // Prove the authority is answering before asserting the negative case.
    server
        .await_answers(&service_qname("e2e-known"), RecordType::A, 1)
        .await;

    let message = server
        .await_code(
            &service_qname("e2e-missing"),
            RecordType::A,
            ResponseCode::NXDomain,
        )
        .await;
    assert!(
        message.answers.is_empty(),
        "an in-zone miss must stay authoritative and never reach an upstream"
    );
}

#[tokio::test]
#[ignore]
async fn e2e_aaaa_for_ipv4_only_service_is_nodata() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-v4only", &[("10.0.0.41", 8080)]).await;

    let server = DoggyDns::start(15414).await;
    let qname = service_qname("e2e-v4only");
    server.await_answers(&qname, RecordType::A, 1).await;

    // The service exists but has no IPv6 address: NOERROR with an empty answer.
    let message = server
        .await_code(&qname, RecordType::AAAA, ResponseCode::NoError)
        .await;
    assert!(
        message.answers.is_empty(),
        "an IPv4-only service must answer AAAA with NODATA"
    );
}

/// The built-in `service_pattern` rule subscribes bare application names only,
/// so a dotted registration is never pushed to this process and its name stays
/// an in-zone miss. Nacos accepts the dotted form (a Dubbo-style identifier),
/// which is exactly why the subscription list has to be narrowed.
#[tokio::test]
#[ignore]
async fn e2e_service_pattern_excludes_dotted_registration() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-selected", &[("10.0.0.51", 8080)]).await;
    nacos.register("e2e.dotted", &[("10.0.0.52", 8080)]).await;

    let server = DoggyDns::start(15415).await;
    // The bare name proves the subscription path is live, so the dotted name's
    // answer below is the filter's doing rather than an empty cache or a
    // start-up race.
    server
        .await_answers(&service_qname("e2e-selected"), RecordType::A, 1)
        .await;

    let message = server
        .await_code(
            &service_qname("e2e.dotted"),
            RecordType::A,
            ResponseCode::NXDomain,
        )
        .await;
    assert!(
        message.answers.is_empty(),
        "a service the pattern rejected must not be subscribed, so it cannot be answered"
    );
}

/// Deleting the last instance must be observed. Reading the client's naming
/// cache per query is what makes this work: an empty push purges the entry (the
/// SDK runs with empty-push protection off), so the next query finds no
/// instance and answers `NXDOMAIN`.
#[tokio::test]
#[ignore]
async fn e2e_deregistered_last_instance_stops_resolving() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-deleted", &[("10.0.0.61", 8080)]).await;

    let server = DoggyDns::start(15416).await;
    let qname = service_qname("e2e-deleted");
    server.await_answers(&qname, RecordType::A, 1).await;

    nacos.deregister("e2e-deleted", "10.0.0.61", 8080).await;

    server
        .await_code(&qname, RecordType::A, ResponseCode::NXDomain)
        .await;
}

/// An unreachable Nacos must answer SERVFAIL. The SDK retries a dead connection
/// in the background and leaves a request pending until it returns, so without
/// the watcher's bound this query would never answer at all; it must also not be
/// NXDOMAIN, which a client would negative-cache past the end of the outage.
#[tokio::test]
#[ignore]
async fn e2e_unreachable_nacos_answers_servfail() {
    // Nothing listens on port 1; the SDK dials the gRPC port (address + 1000).
    let server = DoggyDns::start_with_addr(15417, "127.0.0.1:1").await;

    let message = server
        .await_code(
            &service_qname("cold-outage"),
            RecordType::A,
            ResponseCode::ServFail,
        )
        .await;

    assert!(
        message.answers.is_empty(),
        "SERVFAIL must carry no answer records"
    );
}

/// The first query for a name must be confirmed by the server, never answered
/// from a cache entry this process has not confirmed.
///
/// `naming_load_cache_at_start(true)` makes the client load the previous run's
/// entries, and `get_all_instances(subscribe = true)` answers from that cache
/// before it asks anything — so a loaded entry is served even when the server's
/// list has since moved. Registered here by one process and read back by the
/// next: the disk entry carries the first address, the server the second.
#[tokio::test]
#[ignore]
async fn e2e_first_query_is_confirmed_by_the_server() {
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-stalehole", &[("10.9.9.3", 8080)]).await;

    let cache = DoggyDns::cache_dir(15418);
    let warm = DoggyDns::start_with(15418, &nacos_addr(), &cache).await;
    let qname = service_qname("e2e-stalehole");
    warm.await_answers(&qname, RecordType::A, 1).await;
    let entry = DoggyDns::await_cached_on_disk(&cache, "e2e-stalehole").await;

    // The writer has to go first. A client still subscribed would take the push of
    // the new list and rewrite its own disk entry, leaving nothing stale to read.
    drop(warm);
    nacos.register("e2e-stalehole", &[("10.9.9.4", 8080)]).await;
    // The address below must be the server's, not this process's memory of the
    // registration call: a replacement is not queryable the instant it returns.
    nacos.await_server_has("e2e-stalehole", &["10.9.9.4"]).await;

    let server = DoggyDns::start_with(15419, &nacos_addr(), &cache).await;
    let message = tokio::time::timeout(Duration::from_secs(20), server.send(&qname, RecordType::A))
        .await
        .expect("the first query did not answer within its own bound")
        .expect("no reply to the first query");

    assert_eq!(
        answer_ips(&message),
        HashSet::from(["10.9.9.4".to_string()]),
        "the first query must be answered by the server, not by the entry loaded at {entry:?}"
    );
}
