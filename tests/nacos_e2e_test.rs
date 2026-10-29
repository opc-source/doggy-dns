#![allow(clippy::disallowed_methods)]

//! End-to-end tests for the live Nacos path: `NacosServiceWatcher` -> instance
//! cache -> `NacosAuthority` -> DNS answer.
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
use std::path::PathBuf;
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
            .map(|(ip, port)| ServiceInstance {
                ip: ip.to_string(),
                port: *port,
                healthy: true,
                enabled: true,
                ephemeral: true,
                ..Default::default()
            })
            .collect();
        self.naming
            .batch_register_instance(service.to_string(), Some(GROUP.to_string()), instances)
            .await
            .expect("failed to register instances with Nacos");
    }
}

/// A `doggy-dns` process running the e2e configuration on `port`.
///
/// Subscriptions are established once, at start-up (`subscribe_all`), so every
/// instance the case expects must be registered before this is constructed.
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_doggy-dns"))
            .arg(Self::case_config(port))
            .env("DOGGY_NACOS_ADDR", nacos_addr())
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
        let reply = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buffer))
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
