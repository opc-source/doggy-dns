#![allow(clippy::disallowed_methods)]

//! End-to-end tests for the live Nacos path: `NacosServiceWatcher` -> instance
//! cache -> `NacosAuthority` -> DNS answer, and for the two hot-reload triggers
//! (SIGHUP on a rewritten config file, a push from the Nacos config service)
//! driving that same path through a running process.
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
use nacos_sdk::api::config::{ConfigService, ConfigServiceBuilder};
use nacos_sdk::api::naming::{NamingService, NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::props::ClientProps;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};

const CONFIG_PATH: &str = "config/doggy-dns-nacos-e2e.toml";
/// The same plugin set as `CONFIG_PATH` plus the remote-config subscription the
/// push case reloads through.
const RELOAD_CONFIG_PATH: &str = "config/doggy-dns-nacos-e2e-reload.toml";
/// The remote-config coordinates, kept in sync with `RELOAD_CONFIG_PATH` by
/// hand: the test publishes to what the running process subscribes to.
const RELOAD_GROUP: &str = "DOGGY_DNS";
const RELOAD_DATA_ID: &str = "doggy-dns-e2e-reload.toml";
const ZONE: &str = "nacos.e2e.test";
const GROUP: &str = "DEFAULT_GROUP";
const NAMESPACE: &str = "public";
const RECORD_TTL: u32 = 6;

/// Nacos answers are network round trips; the first subscribe after a cold server
/// start can take a few seconds, so wait generously rather than turning CI jitter
/// into a failure.
const DEADLINE: Duration = Duration::from_secs(60);
/// A remote-config push has to survive the SDK's own polling rhythm before the
/// process can even see it: nacos-sdk 0.8.3 sleeps 60 s between change notices
/// (`src/config/worker.rs:406`), and the publish can land anywhere in a cycle.
/// A deadline of one cycle therefore fails about as often as it passes, which is
/// a coin flip rather than a test — this one covers two full cycles plus the
/// debounce and rebuild.
const PUSH_DEADLINE: Duration = Duration::from_secs(150);
/// How long a SIGHUP reload is given to land. Deliberately far below
/// [`PUSH_DEADLINE`]: the signal path is this process re-reading a file, so if it
/// needed a minute it would be the Nacos poll delivering the change rather than
/// the signal, which is the thing this case is meant to prove.
const SIGHUP_DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(300);
/// How long a reload that must NOT happen is watched for, for a change the
/// process learns about through a signal: a SIGHUP reload lands within the 500 ms
/// debounce window plus one chain build, so a few seconds of unchanged answers is
/// evidence, not luck. A pushed change is not covered by this — see
/// [`PUSH_DEADLINE`] for the window that proves delivery.
const STABLE_WINDOW: Duration = Duration::from_secs(3);

fn nacos_addr() -> String {
    std::env::var("DOGGY_NACOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8848".to_string())
}

/// Patch `key = value` lines of a config: `overrides` rewrites the keys it
/// names, and any port line not named there follows the case's own port.
///
/// `${ENV_VAR}` expansion only reaches string values (see
/// `crates/doggy-dns-basic/src/env.rs`), so the numeric port and ttl cannot come
/// from the environment and are patched in the text instead.
fn rewrite_config(raw: &str, port: u16, overrides: &[(&str, &str)]) -> String {
    raw.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some((key, value)) = overrides
                .iter()
                .find(|(key, _)| trimmed.starts_with(&format!("{key} = ")))
            {
                format!("{key} = {value}")
            } else if trimmed.starts_with("port = ") {
                format!("port = {port}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// One A query against `port`, or `None` when nothing answered in time.
async fn query_port(port: u16, qname: &str, rtype: RecordType) -> Option<Message> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .ok()?;
    socket.connect((Ipv4Addr::LOCALHOST, port)).await.ok()?;

    let query_name = Name::from_ascii(qname).ok()?;
    let mut message = Message::new(0xead0, MessageType::Query, OpCode::Query);
    message.add_query(Query::query(query_name, rtype));
    let bytes = message.to_vec().ok()?;
    socket.send(&bytes).await.ok()?;

    let mut buffer = [0u8; 2048];
    let reply = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut buffer))
        .await
        .ok()?
        .ok()?;
    Message::from_vec(&buffer[..reply]).ok()
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
    /// Where the running process reads its config from, and what it was
    /// generated from. A reload case rewrites the first from the second.
    config_path: PathBuf,
    source: String,
}

impl DoggyDns {
    /// Copy a committed e2e configuration with its port and `overrides`
    /// applied, ready for the process under test to read.
    fn case_config(source: &str, port: u16, overrides: &[(&str, &str)]) -> PathBuf {
        let raw = std::fs::read_to_string(source).expect("failed to read the e2e config");
        let path =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nacos-e2e-{port}.toml"));
        std::fs::write(&path, rewrite_config(&raw, port, overrides))
            .expect("failed to write the case config");
        path
    }

    async fn start(port: u16) -> Self {
        Self::start_from(CONFIG_PATH, port, &[]).await
    }

    async fn start_from(source: &str, port: u16, overrides: &[(&str, &str)]) -> Self {
        let config_path = Self::case_config(source, port, overrides);
        let mut child = Command::new(env!("CARGO_BIN_EXE_doggy-dns"))
            .arg(&config_path)
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

        Self {
            port,
            child,
            config_path,
            source: source.to_string(),
        }
    }

    /// Rewrite this process's config file and `SIGHUP` it: the reload an
    /// operator performs on a host.
    ///
    /// The rewrite regenerates from the committed source, so it stays a
    /// single-field change rather than drifting into whatever a previous case
    /// left in the file.
    async fn reload_via_sighup(&self, overrides: &[(&str, &str)]) {
        let raw = std::fs::read_to_string(&self.source).expect("failed to read the e2e config");
        std::fs::write(
            &self.config_path,
            rewrite_config(&raw, self.port, overrides),
        )
        .expect("failed to rewrite the case config");

        let pid = self
            .child
            .id()
            .expect("doggy-dns must be running to be signalled")
            .to_string();
        let status = std::process::Command::new("kill")
            .arg("-HUP")
            .arg(&pid)
            .status()
            .expect("failed to run kill -HUP");
        assert!(status.success(), "kill -HUP {pid} exited with {status}");
        // The debounce window swallows the signal; the case polls for the
        // effect instead of guessing how long the rebuild took.
    }

    /// Send one query, returning `None` while the server is not answering yet.
    async fn send(&self, qname: &str, rtype: RecordType) -> Option<Message> {
        query_port(self.port, qname, rtype).await
    }

    /// Poll until `predicate` accepts a response, and report what arrived.
    async fn poll_until(
        &self,
        qname: &str,
        rtype: RecordType,
        what: &str,
        predicate: impl Fn(&Message) -> bool,
    ) -> Message {
        self.poll_until_within(qname, rtype, what, DEADLINE, predicate)
            .await
    }

    /// `poll_until` with the wait chosen by the caller, for the effects whose
    /// delivery is paced by something outside this process.
    async fn poll_until_within(
        &self,
        qname: &str,
        rtype: RecordType,
        what: &str,
        deadline: Duration,
        predicate: impl Fn(&Message) -> bool,
    ) -> Message {
        let until = tokio::time::Instant::now() + deadline;
        loop {
            if let Some(message) = self.send(qname, rtype).await
                && predicate(&message)
            {
                return message;
            }
            if tokio::time::Instant::now() >= until {
                panic!("timed out after {deadline:?} waiting for {what} on {qname}");
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

    /// Wait for answers that all carry `want_ttl`.
    ///
    /// TTL is the cheapest proof that a rebuilt chain is answering: it comes
    /// from the plugin config, not from Nacos.
    async fn await_ttl(&self, qname: &str, want_ttl: u32) -> Message {
        self.await_ttl_within(qname, want_ttl, DEADLINE).await
    }

    /// `await_ttl` with the wait chosen by the caller.
    async fn await_ttl_within(&self, qname: &str, want_ttl: u32, deadline: Duration) -> Message {
        self.poll_until_within(
            qname,
            RecordType::A,
            &format!("answers with ttl {want_ttl}"),
            deadline,
            |message| {
                message.metadata.response_code == ResponseCode::NoError
                    && !message.answers.is_empty()
                    && message.answers.iter().all(|record| record.ttl == want_ttl)
            },
        )
        .await
    }

    /// Assert the answer keeps this exact shape for the whole `window`.
    ///
    /// The negative half of a reload test: a change that must have been refused
    /// gets its chance to appear, and every poll inside the window fails the
    /// case if it does.
    async fn assert_still_answering(
        &self,
        qname: &str,
        want_ips: &HashSet<String>,
        want_ttl: u32,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        let mut checks = 0;
        while tokio::time::Instant::now() < deadline {
            let message = self
                .send(qname, RecordType::A)
                .await
                .expect("the running listener must keep answering");
            assert_eq!(
                message.metadata.response_code,
                ResponseCode::NoError,
                "a refused reload must not change the answer"
            );
            assert_eq!(
                &answer_ips(&message),
                want_ips,
                "a refused reload must not change the instances"
            );
            for record in &message.answers {
                assert_eq!(
                    record.ttl, want_ttl,
                    "a refused reload must not change the ttl"
                );
            }
            checks += 1;
            tokio::time::sleep(POLL).await;
        }
        assert!(
            checks >= 3,
            "the window was too short to prove anything: {checks} checks in {window:?}"
        );
    }
}

/// A Nacos config client that publishes the remote config a running process
/// watches, standing in for the operator editing config in the Nacos console.
struct NacosConfigClient {
    service: ConfigService,
}

impl NacosConfigClient {
    async fn connect() -> Self {
        let props = ClientProps::new()
            .server_addr(nacos_addr())
            .namespace(NAMESPACE)
            .app_name("doggy-dns-e2e");
        let service = ConfigServiceBuilder::new(props)
            .build()
            .await
            .expect("failed to build a Nacos config client; is the server running?");
        Self { service }
    }

    /// The case config text a push carries, with `port` and `overrides`
    /// applied. Publishing it is how the case reaches the subscription: the
    /// process never re-reads its own file on this path.
    fn case_config_text(source: &str, port: u16, overrides: &[(&str, &str)]) -> String {
        let raw = std::fs::read_to_string(source).expect("failed to read the e2e config");
        rewrite_config(&raw, port, overrides)
    }

    async fn publish(&self, content: &str) {
        self.service
            .publish_config(
                RELOAD_DATA_ID.to_string(),
                RELOAD_GROUP.to_string(),
                content.to_string(),
                None,
            )
            .await
            .expect("failed to publish the remote config");
    }

    /// Leave nothing behind for the next run: a subscription can deliver the
    /// stored content the moment it is established.
    async fn remove(&self) {
        let _ = self
            .service
            .remove_config(RELOAD_DATA_ID.to_string(), RELOAD_GROUP.to_string())
            .await;
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

/// Answer IPs in wire order — what `rotate_answers` shuffles, so what turning
/// it off has to stop shuffling.
fn answer_order(message: &Message) -> Vec<String> {
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

/// SIGHUP applies a query-path change to the process that is already serving:
/// the ttl and the rotation flag both come from the config, and both move.
#[tokio::test]
#[ignore]
async fn e2e_sighup_reload_applies_plugin_change() {
    let nacos = NacosClient::connect().await;
    nacos
        .register(
            "e2e-reload-sighup",
            &[("10.0.0.61", 8080), ("10.0.0.62", 8080)],
        )
        .await;

    let server = DoggyDns::start(15415).await;
    let qname = service_qname("e2e-reload-sighup");
    let boot = server.await_ttl(&qname, RECORD_TTL).await;
    let instances = answer_ips(&boot);
    assert_eq!(instances.len(), 2, "both instances must be cached first");

    // Rotation on, as the start-up config sets it: consecutive answers trade
    // places. Without this the "order stops changing" assertion below would also
    // pass if rotation had never been enabled at all.
    let mut rotating = Vec::new();
    for _ in 0..4 {
        rotating.push(answer_order(&server.await_ttl(&qname, RECORD_TTL).await));
    }
    assert!(
        rotating.windows(2).any(|pair| pair[0] != pair[1]),
        "rotate_answers = true must be walking the instance list before the reload: {rotating:?}"
    );

    server
        .reload_via_sighup(&[("ttl", "11"), ("rotate_answers", "false")])
        .await;

    let reloaded = server.await_ttl_within(&qname, 11, SIGHUP_DEADLINE).await;
    assert_eq!(
        answer_ips(&reloaded),
        instances,
        "a reload must re-point the query path, not lose the instances"
    );

    // Rotation off is the second half of the same reload: the answer order
    // stops walking and every query looks identical.
    let mut settled = Vec::new();
    for _ in 0..4 {
        settled.push(answer_order(&server.await_ttl(&qname, 11).await));
    }
    assert!(
        settled.windows(2).all(|pair| pair[0] == pair[1]),
        "rotate_answers = false must take effect without a restart: {settled:?}"
    );
}

/// A change a running process cannot make is refused whole — and the half of it
/// that *could* have been applied is refused with it.
#[tokio::test]
#[ignore]
async fn e2e_sighup_rejected_change_keeps_serving() {
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-reload-rejected", &[("10.0.0.71", 8080)])
        .await;

    let server = DoggyDns::start(15416).await;
    let qname = service_qname("e2e-reload-rejected");
    let boot = server.await_ttl(&qname, RECORD_TTL).await;
    let instances = answer_ips(&boot);

    // The candidate moves the listener *and* shortens the ttl: applying the ttl
    // alone would leave a process whose answers disagree with its own config.
    server
        .reload_via_sighup(&[("port", "15599"), ("ttl", "3")])
        .await;

    server
        .assert_still_answering(&qname, &instances, RECORD_TTL, STABLE_WINDOW)
        .await;

    assert!(
        query_port(15599, &qname, RecordType::A).await.is_none(),
        "a rejected reload must not have bound the new port either"
    );
}

/// The remote-config path: a push replaces the query path of a running process,
/// and a push that would move the listener is refused while serving continues.
#[tokio::test]
#[ignore]
async fn e2e_nacos_config_push_reloads() {
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-reload-push", &[("10.0.0.81", 8080)])
        .await;

    let config = NacosConfigClient::connect().await;
    let port = 15417;
    // Publish the start-up content first: a value left from an earlier run would
    // otherwise be what the fresh subscription delivers.
    config
        .publish(&NacosConfigClient::case_config_text(
            RELOAD_CONFIG_PATH,
            port,
            &[],
        ))
        .await;

    let server = DoggyDns::start_from(RELOAD_CONFIG_PATH, port, &[]).await;
    let qname = service_qname("e2e-reload-push");
    let boot = server.await_ttl(&qname, RECORD_TTL).await;
    let instances = answer_ips(&boot);

    config
        .publish(&NacosConfigClient::case_config_text(
            RELOAD_CONFIG_PATH,
            port,
            &[("ttl", "12")],
        ))
        .await;

    let reloaded = server.await_ttl_within(&qname, 12, PUSH_DEADLINE).await;
    assert_eq!(
        answer_ips(&reloaded),
        instances,
        "the pushed config keeps resolving the same service: the nacos connection \
         is reused, not reopened, so nothing has to be re-subscribed"
    );

    // A push that moves the listener is refused; the file on disk is untouched
    // and the running path keeps its ttl.
    //
    // The window has to be the length of a push wait, not the 3 s used against
    // SIGHUP: here the absence of a change is only evidence once the change
    // notice has certainly arrived, and the SDK's 60 s rhythm decides when.
    config
        .publish(&NacosConfigClient::case_config_text(
            RELOAD_CONFIG_PATH,
            port,
            &[("port", "15598"), ("ttl", "20")],
        ))
        .await;
    server
        .assert_still_answering(&qname, &instances, 12, PUSH_DEADLINE)
        .await;

    config.remove().await;
}
