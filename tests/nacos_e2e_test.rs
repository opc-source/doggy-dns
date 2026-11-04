#![allow(clippy::disallowed_methods)]

//! End-to-end tests for the live Nacos path: `NacosServiceWatcher` -> instance
//! cache -> `NacosAuthority` -> DNS answer.
//!
//! Five more cases drive the hot-reload trigger: a configuration published
//! through the config service replaces what the resolver serves (changed, and
//! for an unparsable one not at all), reuses what did not change, and is
//! recorded on disk beside the startup file. They all wait on
//! [`PUSH_DEADLINE`], because nacos-sdk notices config changes on a poll.
//!
//! Every case but one needs a running Nacos server, so they are `#[ignore]`d and
//! never join `cargo test --workspace`. CI runs them in
//! `.github/workflows/nacos-ci.yml`.
//! `e2e_unreachable_nacos_still_boots_and_subscribes_once` is the exception: it
//! dials a port on which nothing listens, which is the point.
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

use doggy_dns::reload::online_record_path;
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use nacos_sdk::api::config::{ConfigService, ConfigServiceBuilder};
use nacos_sdk::api::naming::{NamingService, NamingServiceBuilder, ServiceInstance};
use nacos_sdk::api::props::ClientProps;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};

const CONFIG_PATH: &str = "config/doggy-dns-nacos-e2e.toml";
const ZONE: &str = "nacos.e2e.test";
const GROUP: &str = "DEFAULT_GROUP";
const NAMESPACE: &str = "public";
const RECORD_TTL: u32 = 6;
/// The `ttl` a pushed configuration carries, different enough from
/// [`RECORD_TTL`] to tell the two configurations apart in an answer.
const PUSHED_TTL: u32 = 3;

/// Nacos answers and pushes are network round trips; the first subscribe after
/// a cold server start can take a few seconds, so wait generously rather than
/// turning CI jitter into a failure.
const DEADLINE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(300);

/// How long a pushed configuration may take to arrive.
///
/// nacos-sdk 0.8.3 notices config changes on its `list_ensure_cache_data_newest`
/// poll, which runs every 60 s: the `ConfigChangeNotifyRequest` the server is
/// supposed to send on publish never reached the client in our runs (naming
/// pushes over the same connection do). So a push is a minute, not a
/// millisecond, and it is the only trigger the pipeline has.
const PUSH_DEADLINE: Duration = Duration::from_secs(120);

/// Sampling interval for the gap check that proves a watcher was reused.
///
/// A rebuilt authority owns its zone with an empty cache for the stretch between
/// subscribing and the first instance list (~0.5 s in our runs), so samples have
/// to be closer together than that or an empty window slips through.
const WARM_POLL: Duration = Duration::from_millis(100);

/// How many samples to take after the reload reports itself.
///
/// The empty-cache window a rebuilt authority opens starts at the swap, not at
/// the `config reload applied` line, so the gap check has to keep going past it.
/// This many samples at [`WARM_POLL`] covers several times the ~0.5 s the window
/// was in our runs.
const WARM_SAMPLES_AFTER: usize = 20;

fn nacos_addr() -> String {
    std::env::var("DOGGY_NACOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8848".to_string())
}

/// Remove ANSI colour escapes so a log line can be searched as it reads.
///
/// This is the mechanism, not a guard: the fmt layer colourises whatever its
/// stdout is, a redirected file included, so a field pair reaches the file as
/// `\x1b[3mttl\x1b[0m\x1b[2m=\x1b[0m3` and a substring search for `ttl=3` could
/// never match. Every log read goes through here, and
/// `strip_ansi_leaves_field_pairs_searchable` checks the collapse against a
/// captured colourised line.
fn strip_ansi(raw: &str) -> String {
    let mut clean = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((before, after)) = rest.split_once('\u{1b}') {
        clean.push_str(before);
        // A control sequence is `[`, parameters, then one ASCII letter.
        rest = match after
            .strip_prefix('[')
            .and_then(|params| params.split_once(|c: char| c.is_ascii_alphabetic()))
        {
            Some((_params, tail)) => tail,
            // Anything else is not a sequence this case reads past.
            None => after,
        };
    }
    clean.push_str(rest);
    clean
}

/// A Nacos naming client owned by the test; dropping it unregisters the
/// ephemeral instances registered through it.
///
/// The config service on the same connection is what publishes a pushed
/// configuration for `e2e_config_push_reloads_plugins`.
struct NacosClient {
    naming: NamingService,
    config: ConfigService,
}

impl NacosClient {
    async fn connect() -> Self {
        let props = ClientProps::new()
            .server_addr(nacos_addr())
            .namespace(NAMESPACE)
            .app_name("doggy-dns-e2e");
        let naming = NamingServiceBuilder::new(props.clone())
            .build()
            .await
            .expect("failed to build a Nacos naming client; is the server running?");
        let config = ConfigServiceBuilder::new(props)
            .build()
            .await
            .expect("failed to build a Nacos config client; is the server running?");
        Self { naming, config }
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

    /// Publish `content` as the whole configuration under `data_id`.
    ///
    /// The server notifies every client listening on that id, which is why the
    /// ids are per case: a shared id would let one case's push reload another
    /// case's resolver.
    async fn publish_config(&self, data_id: &str, content: &str) {
        self.config
            .publish_config(
                data_id.to_string(),
                GROUP.to_string(),
                content.to_string(),
                None,
            )
            .await
            .expect("failed to publish a configuration to Nacos");
    }
}

/// A `doggy-dns` process running the e2e configuration on `port`.
///
/// Subscriptions are established once, at start-up (`subscribe_all`), so every
/// instance the case expects must be registered before this is constructed.
///
/// `log` is the file the process writes to, when the case redirected it; the
/// push cases need it because a rejected configuration is only observable in the
/// log (see [`DoggyDns::await_log_line`]).
struct DoggyDns {
    port: u16,
    child: Child,
    log: Option<PathBuf>,
}

impl DoggyDns {
    /// The committed e2e configuration with its `port` line set to `port`.
    ///
    /// `${ENV_VAR}` expansion only reaches string values (see
    /// `crates/doggy-dns-basic/src/env.rs`), so the numeric port cannot come
    /// from the environment and is patched here instead.
    fn patched_config(port: u16) -> String {
        let raw = std::fs::read_to_string(CONFIG_PATH).expect("failed to read the e2e config");
        Self::patch_line(&raw, "port", &port.to_string())
    }

    /// The data id this case's `[remote_config]` trigger listens on.
    fn data_id(port: u16) -> String {
        format!("doggy-dns-e2e-{port}.toml")
    }

    /// Publish `content` as this case's configuration — the push its
    /// `[remote_config]` trigger is listening for.
    async fn publish(&self, nacos: &NacosClient, content: &str) {
        nacos
            .publish_config(&Self::data_id(self.port), content)
            .await;
    }

    /// Publish `content`, sampling `qname` until the reload has been in force
    /// for `WARM_SAMPLES_AFTER` samples; returns `(samples, applied_samples,
    /// blips)`.
    ///
    /// A rebuilt authority owns its zone with an empty cache until its own
    /// subscription reports instances, so a reuse that avoids the rebuild has to
    /// be measured *across* the reload: the window opens when the chain is
    /// swapped, which is one log line before the reload says it applied.
    async fn publish_and_sample(
        &self,
        nacos: &NacosClient,
        content: &str,
        qname: &str,
        served: impl Fn(&Message) -> bool,
    ) -> (usize, usize, usize) {
        self.publish(nacos, content).await;

        let mut samples = 0usize;
        let mut blips = 0usize;
        let mut after_applied = 0usize;
        let until = tokio::time::Instant::now() + PUSH_DEADLINE;
        loop {
            let answered = match self.send(qname, RecordType::A).await {
                Some(message) => served(&message),
                // Silence is the worst kind of blip.
                None => false,
            };
            if !answered {
                blips += 1;
            }
            samples += 1;
            // The first `applied` in this log is this push: the content published
            // before start-up is the file the process loaded, so it only ever
            // skips.
            if self.log_contains("config reload applied") {
                after_applied += 1;
                if after_applied >= WARM_SAMPLES_AFTER {
                    break;
                }
            } else if tokio::time::Instant::now() >= until {
                panic!(
                    "timed out after {PUSH_DEADLINE:?} waiting for the reload of {qname}, \
                     after {samples} samples"
                );
            }
            tokio::time::sleep(WARM_POLL).await;
        }
        (samples, after_applied, blips)
    }

    /// The `[remote_config]` section the four push cases append.
    ///
    /// Committed configs keep the trigger off: the other cases only need the
    /// naming path, and a watcher per case would be one more connection to the
    /// same server for no reason.
    fn remote_config(port: u16) -> String {
        format!(
            "\n[remote_config]\nenabled = true\nserver_addr = \"${{DOGGY_NACOS_ADDR}}\"\nnamespace = \"{NAMESPACE}\"\ngroup = \"{GROUP}\"\ndata_id = \"{}\"\n",
            Self::data_id(port)
        )
    }

    /// `content` with its `key = …` line replaced by `key = value`.
    ///
    /// A push carries a whole configuration, so every variant a case needs is
    /// the same file with one line rewritten.
    fn patch_line(content: &str, key: &str, value: &str) -> String {
        let needle = format!("{key} = ");
        content
            .lines()
            .map(|line| {
                if line.trim_start().starts_with(&needle) {
                    format!("{key} = {value}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<String>>()
            .join("\n")
            + "\n"
    }

    /// `content` with the nacos plugin's `ttl` line set to `ttl`.
    fn with_ttl(content: &str, ttl: u32) -> String {
        Self::patch_line(content, "ttl", &ttl.to_string())
    }

    /// `content` with `[middleware] logging` set to `logging`.
    ///
    /// A field outside `[[plugins]]`: changing it applies the reload while
    /// leaving every plugin entry equal, so it is what a reuse test needs.
    fn with_logging(content: &str, logging: bool) -> String {
        Self::patch_line(content, "logging", &logging.to_string())
    }

    /// The file the resolver on `port` was started from — the case config, whose
    /// sibling is where the record of what is in force lands.
    fn config_path(port: u16) -> PathBuf {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nacos-e2e-{port}.toml"))
    }

    fn write_config(port: u16, content: &str) -> PathBuf {
        let path = Self::config_path(port);
        std::fs::write(&path, content).expect("failed to write the case config");
        path
    }

    async fn start(port: u16) -> Self {
        let content = Self::patched_config(port);
        Self::run(port, &content, None).await
    }

    /// Start the resolver on `port` with `content`, its output redirected to a
    /// log file of its own.
    ///
    /// A rejected configuration changes nothing a DNS query can see — that is
    /// the contract — so the cases that need to know a push *arrived* read the
    /// resolver's log instead.
    async fn start_logged(port: u16, content: &str) -> Self {
        let log = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("nacos-e2e-{port}.log"));
        Self::run(port, content, Some(log)).await
    }

    /// Start the resolver on `port` with `content`; without a log file its
    /// output stays attached, so `--nocapture` shows it in CI and the harness
    /// replays it for a failing test.
    async fn run(port: u16, content: &str, log: Option<PathBuf>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_doggy-dns"));
        command
            .arg(Self::write_config(port, content))
            .env("DOGGY_NACOS_ADDR", nacos_addr())
            // A rejected configuration exits immediately; fail here instead of
            // polling a dead listener until the deadline.
            .kill_on_drop(true);
        if let Some(path) = &log {
            // Redirected: the reload lines are the only witness of a rejected
            // push, and a failing case gets the tail of the file in its panic.
            let stdout = std::fs::File::create(path).expect("failed to create the case log");
            let stderr = stdout
                .try_clone()
                .expect("failed to duplicate the case log");
            command
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
        }
        let mut child = command.spawn().expect("failed to spawn doggy-dns");

        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(status) = child.try_wait().expect("failed to poll doggy-dns") {
            panic!("doggy-dns exited with {status} right after start-up");
        }

        Self { port, child, log }
    }

    /// This process' own log, colour escapes removed.
    fn log_text(&self) -> String {
        let path = self
            .log
            .as_ref()
            .expect("log reads need a case started with start_logged");
        strip_ansi(&std::fs::read_to_string(path).unwrap_or_default())
    }

    /// Whether this process' own log already contains `needle`.
    ///
    /// Absence is only evidence for things the resolver logs *synchronously*
    /// while applying a reload (a retired watcher, for instance).
    fn log_contains(&self, needle: &str) -> bool {
        self.log_text().contains(needle)
    }

    /// Wait until this process' log contains `needle`.
    ///
    /// Waits on [`PUSH_DEADLINE`] because the lines a case waits for are written
    /// when a pushed configuration arrives — and that is on the SDK's poll.
    async fn await_log_line(&self, needle: &str) {
        let until = tokio::time::Instant::now() + PUSH_DEADLINE;
        loop {
            if self.log_contains(needle) {
                return;
            }
            if tokio::time::Instant::now() >= until {
                let tail = self
                    .log_text()
                    .lines()
                    .rev()
                    .take(20)
                    .collect::<Vec<_>>()
                    .join("\n");
                panic!(
                    "timed out after {PUSH_DEADLINE:?} waiting for {needle:?}; last lines:\n{tail}"
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// `kill -HUP` the resolver: the signal an operator used to reload with,
    /// sent to the real process the way it would be sent in anger.
    async fn send_sighup(&self) {
        let pid = self
            .child
            .id()
            .expect("the resolver process is running to be signalled");
        let status = Command::new("kill")
            .arg("-HUP")
            .arg(pid.to_string())
            .status()
            .await
            .expect("failed to run kill -HUP");
        assert!(status.success(), "kill -HUP {pid} failed: {status}");
    }

    /// Panic unless the process is still running.
    fn assert_alive(&mut self) {
        if let Some(status) = self.child.try_wait().expect("failed to poll doggy-dns") {
            panic!("doggy-dns exited with {status}");
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
        deadline: Duration,
        what: &str,
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
        self.poll_until(
            qname,
            rtype,
            DEADLINE,
            &format!("{want} answers"),
            |message| {
                message.answers.len() == want
                    && message.metadata.response_code == ResponseCode::NoError
            },
        )
        .await
    }

    /// Wait for a specific response code.
    async fn await_code(&self, qname: &str, rtype: RecordType, code: ResponseCode) -> Message {
        self.poll_until(
            qname,
            rtype,
            DEADLINE,
            &format!("response code {code:?}"),
            |message| message.metadata.response_code == code,
        )
        .await
    }

    /// Wait for exactly `want` A answers that all carry `ttl`.
    ///
    /// `ttl` is a hot `[plugins]` field baked into the authority, so the record
    /// TTL is what a pushed configuration is visible in — and why this waits on
    /// [`PUSH_DEADLINE`] instead of the subscribe-time deadline the other cases
    /// use.
    async fn await_a_with_ttl(&self, qname: &str, want: usize, ttl: u32) -> Message {
        self.poll_until(
            qname,
            RecordType::A,
            PUSH_DEADLINE,
            &format!("{want} answers with ttl {ttl}"),
            |message| matches_a_with_ttl(message, want, ttl),
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

/// `message` is a NOERROR answer of exactly `want` records, every one of them
/// carrying `ttl`.
fn matches_a_with_ttl(message: &Message, want: usize, ttl: u32) -> bool {
    message.metadata.response_code == ResponseCode::NoError
        && message.answers.len() == want
        && message.answers.iter().all(|record| record.ttl == ttl)
}

/// The record the resolver on this case's port keeps beside its config file, and
/// the panicking read every assertion here goes through: a missing record is
/// this case's failure, not a `None` to unwrap.
fn read_online_record(record: &Path) -> String {
    std::fs::read_to_string(record).expect("the resolver wrote no config online record")
}

/// The backups of `record`, oldest first. Their names end in the epoch seconds
/// they were taken at, all the same width, so the file names sort as their
/// stamps do.
fn online_backups(record: &Path) -> Vec<PathBuf> {
    let dir = record
        .parent()
        .expect("the record sits beside the case config");
    let prefix = format!(
        "{}.bak.",
        record
            .file_name()
            .expect("the record has a name")
            .to_string_lossy()
    );
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the case directory is readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    paths.sort();
    paths
}

/// Remove a record and its backups left over from an earlier run, so a case that
/// counts them starts from a known state.
fn clear_online_record(record: &Path) {
    let _ = std::fs::remove_file(record);
    for backup in online_backups(record) {
        let _ = std::fs::remove_file(backup);
    }
}

/// A live line from a redirected case log, escapes and all.
const COLOURISED_RELOAD_LINE: &str = concat!(
    "\u{1b}[2m2026-11-03T03:56:44.297316Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ",
    "\u{1b}[2mdoggy_dns::reload\u{1b}[0m\u{1b}[2m:\u{1b}[0m config reload applied ",
    "\u{1b}[3mgeneration\u{1b}[0m\u{1b}[2m=\u{1b}[0m1 ",
    "\u{1b}[3mplugins\u{1b}[0m\u{1b}[2m=\u{1b}[0m1 ",
    "\u{1b}[3mlogging\u{1b}[0m\u{1b}[2m=\u{1b}[0mtrue",
);

/// The log reads are what make the push cases possible, so the collapse that
/// lets them match is checked without a Nacos server.
#[test]
fn strip_ansi_leaves_field_pairs_searchable() {
    let clean = strip_ansi(COLOURISED_RELOAD_LINE);
    assert!(
        clean.contains("config reload applied generation=1"),
        "the generation a reload reports must be findable, got {clean}"
    );
    assert!(
        clean.contains("logging=true"),
        "the hot field a reload reports must be findable, got {clean}"
    );
    assert!(
        !clean.contains('\u{1b}'),
        "every escape must be gone, got {clean}"
    );
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

/// Start a resolver whose `[remote_config]` trigger listens on its own data id,
/// with that very content already published.
///
/// A new listener fetches the current content when it registers, so publishing
/// first makes that fetch a no-op — content identical to the one in force is
/// skipped — and every answer change a case observes afterwards sits behind a
/// real push. It also leaves Nacos holding a valid configuration for the next
/// run, whatever a case publishes later.
async fn start_with_remote_config(nacos: &NacosClient, port: u16) -> (String, DoggyDns) {
    let startup = DoggyDns::patched_config(port) + &DoggyDns::remote_config(port);
    nacos
        .publish_config(&DoggyDns::data_id(port), &startup)
        .await;
    let server = DoggyDns::start_logged(port, &startup).await;
    (startup, server)
}

/// A pushed TTL — the change operators actually make — is applied on the
/// connection that is already up: the same warm cache, a new TTL, and no stretch
/// in which the zone answers nothing.
#[tokio::test]
#[ignore]
async fn e2e_push_ttl_only_reuses_the_nacos_connection() {
    let port = 15415;
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-reload", &[("10.0.0.51", 8080), ("10.0.0.52", 8080)])
        .await;

    let (startup, server) = start_with_remote_config(&nacos, port).await;
    let qname = service_qname("e2e-reload");
    server.await_a_with_ttl(&qname, 2, RECORD_TTL).await;

    // Either TTL counts as served: what is being measured is whether the zone
    // ever stops answering, not which configuration was in force for a sample.
    let (samples, after_applied, blips) = server
        .publish_and_sample(
            &nacos,
            &DoggyDns::with_ttl(&startup, PUSHED_TTL),
            &qname,
            |message| {
                matches_a_with_ttl(message, 2, RECORD_TTL)
                    || matches_a_with_ttl(message, 2, PUSHED_TTL)
            },
        )
        .await;

    assert!(
        samples > 20,
        "the push has to have arrived after enough sampling to notice a gap, got {samples}"
    );
    assert_eq!(
        after_applied, WARM_SAMPLES_AFTER,
        "the gap check must outlast the reload it is checking"
    );
    assert_eq!(
        blips, 0,
        "a TTL change keeps the naming connection and the cache behind it, so the zone is \
         never left unserved ({blips} of {samples} samples did not answer)"
    );
    assert!(
        server.log_contains("nacos connection reused"),
        "the reuse is what the reload reports, and the only thing that can explain no gap"
    );
    assert!(
        !server.log_contains("unsubscribed from all nacos services"),
        "reusing the connection must retire nothing"
    );

    // The new TTL is what the zone answers with now.
    let message = server.await_a_with_ttl(&qname, 2, PUSHED_TTL).await;
    assert_eq!(
        answer_ips(&message),
        HashSet::from(["10.0.0.51".to_string(), "10.0.0.52".to_string()]),
        "the reused cache still holds the instances it was pushed"
    );
}

/// A push that cannot be parsed changes nothing: the resolver keeps serving the
/// last good configuration and the trigger still works afterwards.
#[tokio::test]
#[ignore]
async fn e2e_invalid_config_push_keeps_the_running_config() {
    let port = 15416;
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-bad-push", &[("10.0.0.61", 8080), ("10.0.0.62", 8080)])
        .await;

    let (startup, mut server) = start_with_remote_config(&nacos, port).await;
    let qname = service_qname("e2e-bad-push");
    server.await_a_with_ttl(&qname, 2, RECORD_TTL).await;

    server.publish(&nacos, "this is not toml [[[").await;
    // Wait for the resolver to say it rejected the push: until that line is in
    // the log, the bad content may simply not have arrived yet, and publishing
    // anything else sooner lets the SDK poll coalesce the changes so only the
    // newest content is ever handed to the pipeline.
    server.await_log_line("config reload rejected").await;
    server.assert_alive();
    let message = server
        .send(&qname, RecordType::A)
        .await
        .expect("the resolver stopped answering after a rejected push");
    assert!(
        matches_a_with_ttl(&message, 2, RECORD_TTL),
        "a rejected push must leave the configuration in force, got ttls {:?}",
        message
            .answers
            .iter()
            .map(|record| record.ttl)
            .collect::<Vec<_>>()
    );

    // And the rejected push must not have poisoned the watcher: a valid push
    // still reloads.
    server
        .publish(&nacos, &DoggyDns::with_ttl(&startup, PUSHED_TTL))
        .await;
    server.await_a_with_ttl(&qname, 2, PUSHED_TTL).await;
}

/// What a push leaves on disk, and what a signal no longer does: the pushed text
/// is recorded beside the startup file, the text it replaces is kept as one
/// timestamped backup, and `kill -HUP` — no longer a trigger — neither re-reads
/// the file nor takes the process down.
#[tokio::test]
#[ignore]
async fn e2e_pushed_config_is_recorded_and_sighup_changes_nothing() {
    let port = 15417;
    let nacos = NacosClient::connect().await;
    nacos.register("e2e-record", &[("10.0.0.71", 8080)]).await;

    let record = online_record_path(&DoggyDns::config_path(port));
    // The record and its backups live in `target/tmp`, so they outlive a run; a
    // case that counts them has to start from none.
    clear_online_record(&record);

    let (startup, mut server) = start_with_remote_config(&nacos, port).await;
    let qname = service_qname("e2e-record");
    server.await_a_with_ttl(&qname, 1, RECORD_TTL).await;

    assert_eq!(
        record
            .file_name()
            .expect("the record has a name")
            .to_string_lossy(),
        format!("nacos-e2e-{port}.online.toml"),
        "the file an operator reads is named after the config it records"
    );
    // Written at start-up, before any push could arrive: what the process booted
    // with is in force, and it is worth recording even if nothing ever changes.
    assert_eq!(
        read_online_record(&record),
        startup,
        "start-up records the configuration it loaded"
    );
    assert!(
        online_backups(&record).is_empty(),
        "recording for the first time replaces nothing, so it keeps nothing"
    );

    let pushed = DoggyDns::with_ttl(&startup, PUSHED_TTL);
    server.publish(&nacos, &pushed).await;
    server.await_log_line("config reload applied").await;
    server.await_a_with_ttl(&qname, 1, PUSHED_TTL).await;

    // The pushed text, restart-required fields and all, so the next boot applies
    // the whole of it — and the startup file untouched, because the record is a
    // record and never a startup input.
    assert_eq!(read_online_record(&record), pushed);
    assert_eq!(
        std::fs::read_to_string(DoggyDns::config_path(port)).expect("the case config"),
        startup,
        "the file the next boot reads is still the one this boot read"
    );
    let backups = online_backups(&record);
    assert_eq!(
        backups.len(),
        1,
        "the version the push replaced is kept, with the stamp it was replaced at"
    );
    assert_eq!(
        std::fs::read_to_string(&backups[0]).expect("the backup was written"),
        startup
    );

    // The trigger this pipeline dropped: an operator who still reaches for it
    // finds the process alive and the pushed configuration still in force.
    server.send_sighup().await;
    server.await_log_line("SIGHUP ignored").await;
    server.assert_alive();
    // Long enough that a file re-read would have landed and the answer reverted
    // to the startup TTL; the push has to survive it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let message = server
        .send(&qname, RecordType::A)
        .await
        .expect("the resolver stopped answering after SIGHUP");
    assert!(
        matches_a_with_ttl(&message, 1, PUSHED_TTL),
        "a stray kill -HUP must not re-read the startup file, got ttls {:?}",
        message
            .answers
            .iter()
            .map(|record| record.ttl)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        online_backups(&record).len(),
        1,
        "and it must not record anything either"
    );
}

/// A push that changes nothing in `[[plugins]]` reuses the watcher: the zone
/// never blips, nothing is retired, and the field that *did* change takes effect
/// in the running process.
#[tokio::test]
#[ignore]
async fn e2e_push_without_plugin_changes_keeps_the_warm_cache() {
    let port = 15418;
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-warm", &[("10.0.0.81", 8080), ("10.0.0.82", 8080)])
        .await;

    let (startup, server) = start_with_remote_config(&nacos, port).await;
    let qname = service_qname("e2e-warm");
    server.await_a_with_ttl(&qname, 2, RECORD_TTL).await;

    server
        .publish(&nacos, &DoggyDns::with_logging(&startup, true))
        .await;

    let (samples, after_applied, blips) = server
        .publish_and_sample(
            &nacos,
            &DoggyDns::with_logging(&startup, true),
            &qname,
            |message| matches_a_with_ttl(message, 2, RECORD_TTL),
        )
        .await;

    assert!(
        samples > 20,
        "the push has to have arrived after enough sampling to notice a blip, got {samples}"
    );
    assert_eq!(
        after_applied, WARM_SAMPLES_AFTER,
        "the gap check must outlast the reload it is checking"
    );
    assert_eq!(
        blips, 0,
        "an unchanged `[[plugins]]` entry is carried over with its warm cache, not rebuilt \
         ({blips} of {samples} samples did not serve the zone)"
    );
    assert!(
        !server.log_contains("unsubscribed from all nacos services"),
        "reusing every entry must retire nothing"
    );
    assert!(
        server.log_contains("logging=true"),
        "the reload reports the hot field it applied"
    );

    // And the middleware that field installs is in the request path now, which
    // is visible in the one query that follows the reload.
    server.send(&qname, RecordType::A).await;
    server.await_log_line("dns query completed").await;
}

/// A connection the new configuration does not ask for is unsubscribed rather
/// than left behind, and the one it does ask for is opened and refilled.
///
/// `127.0.0.1:8848` and `localhost:8848` are the same server, so the reload can
/// succeed — but they are different identities, so nothing is carried over. This
/// is the half the pooling rule needs: reuse where the identity holds, and a
/// real teardown where it does not.
#[tokio::test]
#[ignore]
async fn e2e_push_a_different_server_addr_retires_the_old_connection() {
    let port = 15419;
    let nacos = NacosClient::connect().await;
    nacos
        .register("e2e-reconnect", &[("10.0.0.91", 8080), ("10.0.0.92", 8080)])
        .await;

    let (startup, server) = start_with_remote_config(&nacos, port).await;
    let qname = service_qname("e2e-reconnect");
    server.await_a_with_ttl(&qname, 2, RECORD_TTL).await;

    // `patch_line` rewrites every `server_addr` line, so `[remote_config]` moves
    // too: that field is not reloadable, and the same reload has to say so.
    server
        .publish(
            &nacos,
            &DoggyDns::patch_line(&startup, "server_addr", "\"localhost:8848\""),
        )
        .await;

    server
        .await_log_line("unsubscribed from all nacos services")
        .await;
    assert!(
        !server.log_contains("nacos connection reused"),
        "a different identity must not be matched to the old connection"
    );
    assert!(
        server.log_contains("these configuration changes need a restart"),
        "and the field a running process cannot change is reported, not applied"
    );

    // The fresh subscription re-reads the instances, so the zone is served again
    // from the new connection.
    let message = server.await_a_with_ttl(&qname, 2, RECORD_TTL).await;
    assert_eq!(
        answer_ips(&message),
        HashSet::from(["10.0.0.91".to_string(), "10.0.0.92".to_string()]),
        "the rebuilt authority must serve the instances from its own connection"
    );
}

/// A Nacos that is not up yet costs the resolver nothing.
///
/// Both clients — the naming one behind the zone and the `[remote_config]`
/// trigger — start in emergency mode, so the process boots, binds and serves its
/// zone (empty, so authoritatively nothing) instead of exiting on a connection
/// error. And the trigger's watcher is built once, not per attempt: a container
/// whose Nacos answers a minute later gets one subscription that fires when the
/// server appears, rather than a client rebuilt every 30 s with its SDK worker
/// tasks left behind each time.
///
/// This case needs no Nacos server; it is `#[ignore]`d because it belongs to the
/// suite the Nacos CI job runs, and because the port it dials must stay free.
#[tokio::test]
#[ignore]
async fn e2e_unreachable_nacos_still_boots_and_subscribes_once() {
    let port = 15420;

    let startup = DoggyDns::patched_config(port) + &DoggyDns::remote_config(port);
    // Every `server_addr` line, so the zone's naming client and the config
    // trigger are both pointed at an address nothing answers on. The SDK dials
    // gRPC at this port plus 1000, so neither has a listener.
    let startup = DoggyDns::patch_line(&startup, "server_addr", "\"127.0.0.1:18848\"");
    let mut server = DoggyDns::start_logged(port, &startup).await;

    // The naming side reached its soft failure: the client exists, the service
    // list could not be read, and startup carried on regardless.
    server.await_log_line("failed to list nacos services").await;
    // The trigger subscribed even though the server never answered its health
    // check — and exactly once, which is what the old 30 s rebuild loop did not
    // give.
    server
        .await_log_line("config reload listening on nacos push")
        .await;
    assert_eq!(
        server
            .log_text()
            .matches("config reload listening on nacos push")
            .count(),
        1,
        "one watcher for the life of the process, not one per retry"
    );
    assert!(
        !server.log_contains("remote config watcher unavailable"),
        "an unreachable server is not a lost trigger"
    );

    server.assert_alive();
    // Serving the zone from an empty cache is the proof the chain was built: the
    // alternative was a process that exited before it bound.
    server
        .await_code(
            &service_qname("e2e-not-yet"),
            RecordType::A,
            ResponseCode::NXDomain,
        )
        .await;
}
