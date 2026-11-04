#![allow(clippy::disallowed_methods)]

//! The reload pipeline as its trigger sees it: one entry point, the same
//! guarantees for every pushed configuration. These drive the real coordinator,
//! so what a rejected push leaves behind is the production code path, not a
//! mock.

use doggy_dns::reload::{
    Applied, PluginSlot, PreviousEntry, ReloadCoordinator, ReloadOutcome, SlotPlan,
    authority_chain_from, build_chain, middlewares_for, nacos_zone_signals, online_record_path,
    plan_slots, restart_required, run_reload_loop, write_online_record,
};
use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::{DoggyDnsConfig, PluginConfig, PluginKind, parse_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_plugin::Middleware;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const STARTUP: &str = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1
tcp_timeout = 10
shutdown_timeout = 2
rotate_answers = true

[middleware]
logging = true

[[plugins]]
kind = "native"
enabled = true
cache_size = 4096
min_ttl = 10
max_ttl = 300

[observability]
enabled = false
"#;

/// `STARTUP` with every hot field changed: rotation off, logging off, and the
/// plugin replaced by a forward one.
const HOT_CHANGES: &str = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1
tcp_timeout = 10
shutdown_timeout = 2
rotate_answers = false

[middleware]
logging = false

[[plugins]]
kind = "forward"
enabled = true
upstream = ["127.0.0.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

[observability]
enabled = false
"#;

/// `STARTUP` plus a `tcp_timeout` change: the hot set untouched, one field that
/// needs a restart.
const RESTART_ONLY: &str = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1
tcp_timeout = 42
shutdown_timeout = 2
rotate_answers = true

[middleware]
logging = true

[[plugins]]
kind = "native"
enabled = true
cache_size = 4096
min_ttl = 10
max_ttl = 300

[observability]
enabled = false
"#;

const TWO_PLUGINS: &str = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1

[[plugins]]
kind = "native"
enabled = true
cache_size = 4096
min_ttl = 10
max_ttl = 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["127.0.0.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600
"#;

/// A config path the record writer can write to: one file per call under
/// `target/tmp/reload-<pid>`, so runs do not collide and a test can count the
/// backups of its own record.
fn scratch_config_path(label: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/tmp")
        .join(format!("reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory for the online record");
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dir.join(format!("{label}-{seq}.toml"))
}

/// The startup sequence from `main.rs`: build the chain, hand it to the
/// handler, adopt both in the coordinator.
async fn coordinator_for(
    content: &str,
) -> (ReloadCoordinator, Arc<DoggyDnsHandler>, Arc<LiveState>) {
    coordinator_at(content, scratch_config_path("cfg")).await
}

/// The same, at a config path the caller keeps — for the tests that read back
/// the record the pipeline writes.
async fn coordinator_at(
    content: &str,
    config_path: PathBuf,
) -> (ReloadCoordinator, Arc<DoggyDnsHandler>, Arc<LiveState>) {
    let config = parse_config(content).expect("startup config parses");
    let mut previous: Vec<PluginSlot> = Vec::new();
    let slots = build_chain(&config, &mut previous)
        .await
        .expect("chain builds");

    let base: Vec<Arc<dyn Middleware>> = Vec::new();
    let handler = Arc::new(DoggyDnsHandler::new(
        middlewares_for(config.middleware.logging, &base),
        Arc::new(authority_chain_from(&slots)),
        false,
        AnswerRotation::new(config.server.rotate_answers),
    ));

    let live = Arc::new(LiveState::new());
    live.replace_nacos_zones(nacos_zone_signals(&slots));

    let coordinator = ReloadCoordinator::new(
        config_path,
        Applied {
            raw: content.to_string(),
            config,
            slots,
            generation: 0,
        },
        Arc::clone(&handler),
        base,
        None,
        Arc::clone(&live),
    );
    (coordinator, handler, live)
}

async fn slots_for(content: &str, previous: &mut [PluginSlot]) -> Vec<PluginSlot> {
    build_chain(&parse_config(content).unwrap(), previous)
        .await
        .expect("chain builds")
}

fn config_of(content: &str) -> DoggyDnsConfig {
    parse_config(content).expect("test config parses")
}

/// The backups of `record`, oldest first. Their names end in the epoch seconds
/// they were taken at, all the same width, so the file names sort as their
/// stamps do.
fn online_backups(record: &Path) -> Vec<PathBuf> {
    let dir = record.parent().expect("the record sits in a directory");
    let prefix = format!(
        "{}.bak.",
        record
            .file_name()
            .expect("the record has a name")
            .to_string_lossy()
    );
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the record's directory is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    names.sort();
    names.into_iter().map(|name| dir.join(name)).collect()
}

// --- the coordinator ---

#[tokio::test]
async fn hot_fields_are_applied_without_a_restart() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;
    assert!(handler.rotation_enabled());
    assert_eq!(handler.middlewares().len(), 1, "logging is on at startup");

    let outcome = coordinator.apply_reload(HOT_CHANGES).await;

    assert_eq!(outcome, ReloadOutcome::Applied { generation: 1 });
    assert!(
        !handler.rotation_enabled(),
        "server.rotate_answers is hot: the running handler must reflect it"
    );
    assert!(
        handler.middlewares().is_empty(),
        "middleware.logging is hot: turning it off removes the middleware from the request path"
    );
    let applied = coordinator.applied_config().await;
    assert!(!applied.middleware.logging);
    assert_eq!(applied.plugins.len(), 1);
    assert_eq!(applied.plugins[0].kind, PluginKind::Forward);
}

#[tokio::test]
async fn invalid_toml_is_rejected_and_leaves_the_running_config_in_force() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;

    let outcome = coordinator.apply_reload("this = = not toml").await;

    assert_eq!(outcome, ReloadOutcome::Rejected);
    // Nothing observable changed: still the startup config, still rotation on.
    assert!(handler.rotation_enabled());
    assert_eq!(handler.middlewares().len(), 1);
    assert_eq!(
        coordinator.applied_config().await.plugins[0].kind,
        PluginKind::Native
    );
}

#[tokio::test]
async fn validation_failure_is_rejected_too() {
    // Parses cleanly, fails `validate_config` (worker_threads must be > 0): the
    // whole reload is rejected, not partially applied.
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;
    let broken = STARTUP.replace("worker_threads = 1", "worker_threads = 0");

    let outcome = coordinator.apply_reload(&broken).await;

    assert_eq!(outcome, ReloadOutcome::Rejected);
    assert!(
        handler.rotation_enabled(),
        "the old config is still in force"
    );
    assert_eq!(
        coordinator.applied_config().await.server.worker_threads,
        1,
        "a rejected reload records nothing, so a corrected push is not skipped as identical"
    );
}

#[tokio::test]
async fn identical_content_is_skipped() {
    let (coordinator, _handler, _live) = coordinator_for(STARTUP).await;

    let outcome = coordinator.apply_reload(STARTUP).await;

    assert_eq!(
        outcome,
        ReloadOutcome::Skipped,
        "re-pushing the content already in force must not rebuild the chain"
    );
}

#[tokio::test]
async fn a_restart_required_change_is_reported_and_kept_out_of_the_baseline() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;

    let outcome = coordinator.apply_reload(RESTART_ONLY).await;

    // The reload succeeds: the hot set is unchanged, the text is recorded so the
    // same push is not re-diffed, and the warning is what the operator gets.
    assert_eq!(outcome, ReloadOutcome::Applied { generation: 1 });
    assert!(handler.rotation_enabled());
    assert_eq!(
        coordinator.applied_config().await.server.tcp_timeout,
        10,
        "the baseline records what is in force, not what was pushed: the listener still has 10"
    );
    assert_eq!(
        restart_required(
            &coordinator.applied_config().await,
            &config_of(RESTART_ONLY)
        ),
        vec!["server.tcp_timeout".to_string()],
        "and it keeps reporting it, so the same edit is not silently absorbed once and forgotten"
    );
}

#[tokio::test]
async fn hot_paths_are_never_reported_as_restart_required() {
    // Everything that differs between STARTUP and HOT_CHANGES is hot.
    assert!(restart_required(&config_of(STARTUP), &config_of(HOT_CHANGES)).is_empty());
}

#[tokio::test]
async fn a_new_config_section_is_reported_without_a_field_list() {
    // The diff walks the serialised configs, so a path nobody listed is still
    // caught. `control` is off in both by default; enabling it changes values.
    let with_control = STARTUP.replace(
        "[observability]\nenabled = false",
        "[observability]\nenabled = false\n\n[control]\nenabled = true\nlisten_addr = \"127.0.0.1\"\nport = 19100\nendpoints = [\"readyz\"]",
    );
    let paths = restart_required(&config_of(STARTUP), &config_of(&with_control));
    assert!(
        paths.iter().any(|path| path.starts_with("control.")),
        "expected control.* paths, got {paths:?}"
    );
}

#[tokio::test]
async fn a_section_the_process_never_opened_stays_out_of_the_baseline() {
    // The same difference on the other side of a reload. `[server.tls]` is absent
    // in force and not a hot path, and the process has no DoT listener, so
    // absorbing it into the baseline would silence the report after one push.
    // An `Option` field is what makes this a different shape from the
    // value-level case above: the path exists on one side of the tree only.
    let with_tls = STARTUP.replace(
        "[middleware]\nlogging = true",
        "[server.tls]\nenabled = false\ncert_path = \"/unused/cert.pem\"\nkey_path = \"/unused/key.pem\"\n\n[middleware]\nlogging = true",
    );
    assert!(
        config_of(STARTUP).server.tls.is_none() && config_of(&with_tls).server.tls.is_some(),
        "the fixture has to add the section, not just change a value in it"
    );

    let (coordinator, _handler, _live) = coordinator_for(STARTUP).await;
    assert_eq!(
        coordinator.apply_reload(&with_tls).await,
        ReloadOutcome::Applied { generation: 1 },
        "a restart-required-only push still applies the hot set, which here is nothing"
    );

    let applied = coordinator.applied_config().await;
    assert!(
        applied.server.tls.is_none(),
        "the baseline records what is in force: no TLS listener was opened"
    );
    let paths = restart_required(&applied, &config_of(&with_tls));
    assert!(
        paths.iter().all(|path| path.starts_with("server.tls")) && !paths.is_empty(),
        "and it reports that section, and only it, on every later reload: {paths:?}"
    );
}

#[tokio::test]
async fn the_first_plugins_entry_moves_into_the_baseline() {
    // The mirror image, and the guard against tightening the rule too far: the
    // chain a reload built *is* in force, so a hot path arriving where there was
    // none has to be adopted — otherwise the next reload reports `plugins` as a
    // field it cannot apply.
    let no_plugins = STARTUP
        .replace("\n[[plugins]]\nkind = \"native\"\nenabled = true\ncache_size = 4096\nmin_ttl = 10\nmax_ttl = 300\n", "");
    assert_eq!(config_of(&no_plugins).plugins.len(), 0);

    let (coordinator, _handler, _live) = coordinator_for(&no_plugins).await;
    assert_eq!(
        coordinator.apply_reload(STARTUP).await,
        ReloadOutcome::Applied { generation: 1 }
    );

    let applied = coordinator.applied_config().await;
    assert_eq!(
        applied.plugins.len(),
        1,
        "the entry the reload built is in force"
    );
    assert_eq!(applied.plugins[0].kind, PluginKind::Native);
    assert!(
        restart_required(&applied, &config_of(STARTUP)).is_empty(),
        "a hot path is never left as a pending restart"
    );
}

// --- chain building ---

#[tokio::test]
async fn unchanged_entries_keep_their_handler_so_caches_stay_warm() {
    let mut previous: Vec<PluginSlot> = Vec::new();
    let first = slots_for(TWO_PLUGINS, &mut previous).await;
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].name(), "native");
    assert_eq!(first[1].name(), "forward[127.0.0.1:53]");
    let native = Arc::clone(first[0].handler().expect("enabled plugin"));
    let forward = Arc::clone(first[1].handler().expect("enabled plugin"));

    // Same content through the builder again: both handlers are the very same
    // objects, which is what keeps a warm cache warm.
    let mut previous = first;
    let second = slots_for(TWO_PLUGINS, &mut previous).await;
    assert!(Arc::ptr_eq(&native, second[0].handler().unwrap()));
    assert!(Arc::ptr_eq(&forward, second[1].handler().unwrap()));
}

#[tokio::test]
async fn a_param_change_rebuilds_only_that_entry() {
    let mut previous: Vec<PluginSlot> = Vec::new();
    let first = slots_for(TWO_PLUGINS, &mut previous).await;
    let native = Arc::clone(first[0].handler().unwrap());
    let forward = Arc::clone(first[1].handler().unwrap());

    let changed = TWO_PLUGINS.replace(
        "upstream = [\"127.0.0.1:53\"]",
        "upstream = [\"1.0.0.1:53\"]",
    );
    let mut previous = first;
    let second = slots_for(&changed, &mut previous).await;

    assert!(
        Arc::ptr_eq(&native, second[0].handler().unwrap()),
        "the unchanged prefix entry is carried over"
    );
    assert!(
        !Arc::ptr_eq(&forward, second[1].handler().unwrap()),
        "the entry whose upstream changed is rebuilt"
    );
    assert_eq!(second[1].name(), "forward[1.0.0.1:53]");
}

#[tokio::test]
async fn a_middle_insert_rebuilds_everything_after_it() {
    // The documented cost of a position-wise diff (design §8.1).
    let inserted = TWO_PLUGINS.replace(
        "[[plugins]]\nkind = \"forward\"",
        "[[plugins]]\nkind = \"native\"\ncache_size = 100\nmin_ttl = 1\nmax_ttl = 2\n\n[[plugins]]\nkind = \"forward\"",
    );
    let mut previous: Vec<PluginSlot> = Vec::new();
    let first = slots_for(TWO_PLUGINS, &mut previous).await;
    let native = Arc::clone(first[0].handler().unwrap());

    let mut previous = first;
    let second = slots_for(&inserted, &mut previous).await;
    assert_eq!(second.len(), 3);
    assert!(Arc::ptr_eq(&native, second[0].handler().unwrap()));
    assert_eq!(second[1].name(), "native");
    assert_eq!(second[2].name(), "forward[127.0.0.1:53]");
}

#[tokio::test]
async fn a_shrink_retires_the_surplus_entries() {
    let shrunk = TWO_PLUGINS
        .split("\n\n[[plugins]]\nkind = \"forward\"")
        .next()
        .unwrap();
    let mut previous: Vec<PluginSlot> = Vec::new();
    let first = slots_for(TWO_PLUGINS, &mut previous).await;
    assert_eq!(first.len(), 2);

    let mut previous = first;
    let second = slots_for(shrunk, &mut previous).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].name(), "native");
}

#[tokio::test]
async fn no_plugins_still_builds_the_default_forward_chain() {
    let bare = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1
"#;
    let mut previous: Vec<PluginSlot> = Vec::new();
    let slots = slots_for(bare, &mut previous).await;
    assert_eq!(slots.len(), 1);
    assert_eq!(
        slots[0].name(),
        "forward[default]",
        "the zero-config chain keeps the name the answers and reports already show"
    );
    let default_handler = Arc::clone(slots[0].handler().expect("the default chain resolves"));

    // And it is reusable: a reload that still has no plugins carries the very
    // same authority over, so its forward cache survives.
    let mut previous = slots;
    let again = slots_for(bare, &mut previous).await;
    assert_eq!(again[0].name(), "forward[default]");
    assert!(
        Arc::ptr_eq(&default_handler, again[0].handler().unwrap()),
        "the implicit chain is carried over, not rebuilt"
    );

    // The boundary the implicit entry must never cross: writing the same
    // forwarders out explicitly is a different plugin (its tuning comes from the
    // serde defaults, not from the implicit chain's), so it is rebuilt.
    let explicit = bare.to_string() + "\n[[plugins]]\nkind = \"forward\"\n";
    let mut previous = again;
    let explicit_slots = slots_for(&explicit, &mut previous).await;
    assert!(
        !Arc::ptr_eq(&default_handler, explicit_slots[0].handler().unwrap()),
        "an explicit entry is never carried over as the implicit one"
    );
    assert_eq!(explicit_slots[0].name(), "forward[8.8.8.8:53,1.1.1.1:53]");
}

#[tokio::test]
async fn a_disabled_plugin_contributes_no_handler() {
    let disabled = TWO_PLUGINS.replace(
        "kind = \"native\"\nenabled = true",
        "kind = \"native\"\nenabled = false",
    );
    let mut previous: Vec<PluginSlot> = Vec::new();
    let slots = slots_for(&disabled, &mut previous).await;

    assert_eq!(slots.len(), 2, "the slot still describes the config entry");
    assert!(slots[0].handler().is_none());
    assert!(!authority_chain_from(&slots).is_empty());
}

#[tokio::test]
async fn a_chain_of_native_and_forward_has_no_nacos_zone_signal() {
    // The live zone signals read the watchers' caches; a chain without a nacos
    // plugin must report none, so `/readyz` cannot invent a zone.
    let mut previous: Vec<PluginSlot> = Vec::new();
    let slots = slots_for(TWO_PLUGINS, &mut previous).await;
    assert!(nacos_zone_signals(&slots).is_empty());
}

#[tokio::test]
async fn a_plugin_that_cannot_be_built_leaves_the_previous_slots_untouched() {
    // `build_chain` consumes `previous` only once every fallible step
    // succeeded, so a failed build cannot empty the caller's chain.
    let unreachable = r#"
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 1

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:1"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6
"#;
    let mut previous: Vec<PluginSlot> = Vec::new();
    let first = slots_for(TWO_PLUGINS, &mut previous).await;
    let native = Arc::clone(first[0].handler().unwrap());

    let mut previous = first;
    let error = match build_chain(&config_of(unreachable), &mut previous).await {
        Ok(slots) => panic!(
            "nothing listens on 127.0.0.1:1, yet {} slots were built",
            slots.len()
        ),
        Err(error) => error,
    };
    assert!(
        Arc::ptr_eq(&native, previous[0].handler().unwrap()),
        "the caller still owns the running chain after a build failure: {error}"
    );
    assert_eq!(previous.len(), 2);
}

// --- reuse planning ---

/// A nacos plugin with every connection field written out.
fn nacos_plugin(server_addr: &str, zone: &str, ttl: u32) -> PluginConfig {
    PluginConfig {
        kind: PluginKind::Nacos,
        enabled: true,
        server_addr: Some(server_addr.to_string()),
        namespace: Some("public".to_string()),
        group: Some("DEFAULT_GROUP".to_string()),
        dns_zone: Some(zone.to_string()),
        ttl,
        cache_size: 1024,
        min_ttl: 30,
        max_ttl: 600,
        upstream: None,
    }
}

/// A previous slot that still owns its naming connection.
fn pooled(plugin: PluginConfig) -> PreviousEntry {
    PreviousEntry {
        plugin,
        has_connection: true,
    }
}

#[tokio::test]
async fn an_unchanged_entry_is_carried_and_nothing_else_matches_it() {
    let plugin = nacos_plugin("127.0.0.1:8848", "a.local", 6);
    let previous = pooled(plugin.clone());
    let plans = plan_slots(
        std::slice::from_ref(&plugin),
        std::slice::from_ref(&previous),
    );
    assert_eq!(plans, vec![SlotPlan::Carry(0)]);
}

#[tokio::test]
async fn a_ttl_change_keeps_the_connection() {
    // The change operators actually make. A rebuild would mean a new connection,
    // a fresh subscription, and a zone nobody answers until the instances arrive
    // again; reuse means a struct.
    let plans = plan_slots(
        &[nacos_plugin("127.0.0.1:8848", "a.local", 3)],
        &[pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6))],
    );
    assert_eq!(plans, vec![SlotPlan::ReuseConnection(0)]);
}

#[tokio::test]
async fn a_zone_change_keeps_the_connection() {
    // The cache is keyed by service/group/namespace, so a new zone publishes the
    // same instances without refilling anything.
    let plans = plan_slots(
        &[nacos_plugin("127.0.0.1:8848", "b.local", 6)],
        &[pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6))],
    );
    assert_eq!(plans, vec![SlotPlan::ReuseConnection(0)]);
}

#[tokio::test]
async fn a_connection_change_builds_and_leaves_the_old_slot_to_retire() {
    let previous = pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6));

    let mut other_server = nacos_plugin("localhost:8848", "a.local", 6);
    other_server.namespace = Some("public".to_string());
    let mut other_namespace = nacos_plugin("127.0.0.1:8848", "a.local", 6);
    other_namespace.namespace = Some("prod".to_string());
    let mut other_group = nacos_plugin("127.0.0.1:8848", "a.local", 6);
    other_group.group = Some("OTHER_GROUP".to_string());

    for changed in [other_server, other_namespace, other_group] {
        assert_eq!(
            plan_slots(&[changed], std::slice::from_ref(&previous)),
            vec![SlotPlan::Build],
            "server addr, namespace and group are the identity; anything else is not"
        );
    }
}

#[tokio::test]
async fn unset_fields_are_the_same_identity_as_the_documented_defaults() {
    let unset = PluginConfig {
        kind: PluginKind::Nacos,
        enabled: true,
        server_addr: None,
        namespace: None,
        group: None,
        dns_zone: Some("a.local".to_string()),
        ttl: 3,
        cache_size: 1024,
        min_ttl: 30,
        max_ttl: 600,
        upstream: None,
    };
    let plans = plan_slots(
        &[unset],
        &[pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6))],
    );
    assert_eq!(plans, vec![SlotPlan::ReuseConnection(0)]);
}

#[tokio::test]
async fn matching_consumes_so_two_plugins_on_one_server_take_two_connections() {
    let previous = vec![
        pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6)),
        pooled(nacos_plugin("127.0.0.1:8848", "b.local", 6)),
    ];
    let desired = [
        nacos_plugin("127.0.0.1:8848", "a.local", 3),
        nacos_plugin("127.0.0.1:8848", "b.local", 4),
    ];
    assert_eq!(
        plan_slots(&desired, &previous),
        vec![SlotPlan::ReuseConnection(0), SlotPlan::ReuseConnection(1)]
    );

    // One pooled connection cannot serve two plugins, so the second opens its own.
    assert_eq!(
        plan_slots(&desired, &previous[..1]),
        vec![SlotPlan::ReuseConnection(0), SlotPlan::Build]
    );
}

#[tokio::test]
async fn a_connection_is_matched_by_identity_not_by_index() {
    // A forward plugin inserted in front keeps the nacos connection it still
    // asks for, even though its index moved.
    let forward = PluginConfig {
        kind: PluginKind::Forward,
        enabled: true,
        server_addr: None,
        namespace: None,
        group: None,
        dns_zone: None,
        ttl: 6,
        cache_size: 1024,
        min_ttl: 30,
        max_ttl: 600,
        upstream: Some(vec!["127.0.0.1:53".to_string()]),
    };
    let plans = plan_slots(
        &[forward, nacos_plugin("127.0.0.1:8848", "a.local", 3)],
        &[pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6))],
    );
    assert_eq!(plans, vec![SlotPlan::Build, SlotPlan::ReuseConnection(0)]);
}

#[tokio::test]
async fn reordered_connections_go_to_their_own_plugins() {
    // Two servers, order flipped. Taking the first free connection and only then
    // checking its identity would hand both plugins a rebuild; the match has to
    // look for the connection each plugin actually asks for.
    let a = nacos_plugin("127.0.0.1:8848", "a.local", 6);
    let b = nacos_plugin("localhost:8848", "a.local", 6);
    let previous = vec![pooled(a.clone()), pooled(b.clone())];
    let plans = plan_slots(&[b, a], &previous);
    assert_eq!(
        plans,
        vec![SlotPlan::ReuseConnection(1), SlotPlan::ReuseConnection(0)],
        "each entry reuses the connection it wants, wherever it sits in the pool"
    );
}

#[tokio::test]
async fn a_disabled_plugin_claims_no_connection() {
    let mut off = nacos_plugin("127.0.0.1:8848", "a.local", 6);
    off.enabled = false;
    let plans = plan_slots(
        &[off],
        &[pooled(nacos_plugin("127.0.0.1:8848", "a.local", 6))],
    );
    assert_eq!(plans, vec![SlotPlan::Build]);
}

#[tokio::test]
async fn an_empty_previous_chain_builds_everything() {
    let plans = plan_slots(&[nacos_plugin("127.0.0.1:8848", "a.local", 6)], &[]);
    assert_eq!(plans, vec![SlotPlan::Build]);
}

// --- counting and coalescing ---

#[tokio::test]
async fn reloads_are_counted_by_generation() {
    let (coordinator, _handler, _live) = coordinator_for(STARTUP).await;
    assert_eq!(coordinator.generation().await, 0);

    let outcome = coordinator.apply_reload(HOT_CHANGES).await;
    assert_eq!(outcome, ReloadOutcome::Applied { generation: 1 });

    // Neither a skip nor a rejection moves the count: it counts reloads that
    // actually replaced the query path.
    assert_eq!(
        coordinator.apply_reload(HOT_CHANGES).await,
        ReloadOutcome::Skipped
    );
    assert_eq!(
        coordinator.apply_reload("this = = not toml").await,
        ReloadOutcome::Rejected
    );
    assert_eq!(coordinator.generation().await, 1);

    assert_eq!(
        coordinator.apply_reload(RESTART_ONLY).await,
        ReloadOutcome::Applied { generation: 2 },
        "a change that needs a restart still counts as a reload"
    );
    assert_eq!(coordinator.generation().await, 2);
}

#[tokio::test]
async fn a_burst_of_pushes_folds_into_the_newest() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;
    let coordinator = Arc::new(coordinator);
    let (pushes, queue) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(run_reload_loop(Arc::clone(&coordinator), queue));

    // Both are queued before the loop can look at either, so the burst is one
    // reload of the newest content: rotation and logging stay as the startup
    // config has them, which is what the second push asks for and what the
    // first would have turned off.
    pushes
        .send(HOT_CHANGES.to_string())
        .expect("the reload loop is listening");
    pushes
        .send(RESTART_ONLY.to_string())
        .expect("the reload loop is listening");

    for _ in 0..100 {
        if coordinator.generation().await > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert_eq!(
        coordinator.generation().await,
        1,
        "two queued pushes, one reload"
    );
    assert!(
        handler.rotation_enabled(),
        "the newest push is the one that was applied"
    );
    assert_eq!(
        handler.middlewares().len(),
        1,
        "and the newest push kept the logging middleware"
    );
}

// --- the record of what is in force ---

#[tokio::test]
async fn the_pushed_text_lands_in_the_record_and_the_old_version_in_a_backup() {
    let config_path = scratch_config_path("record");
    let record = online_record_path(&config_path);
    // What startup writes for itself, so this case starts the way a real
    // process does: a record already in place.
    write_online_record(&config_path, STARTUP);

    let (coordinator, _handler, _live) = coordinator_at(STARTUP, config_path.clone()).await;
    assert_eq!(
        coordinator.apply_reload(HOT_CHANGES).await,
        ReloadOutcome::Applied { generation: 1 }
    );

    assert_eq!(
        std::fs::read_to_string(&record).expect("the record was written"),
        HOT_CHANGES,
        "the record is the pushed text, restart-required fields and all, so the next boot applies them"
    );
    let backups = online_backups(&record);
    assert_eq!(backups.len(), 1, "the version it replaced was kept");
    assert_eq!(
        std::fs::read_to_string(&backups[0]).expect("the backup was written"),
        STARTUP,
        "and it is the version that was in force before this push"
    );
}

#[tokio::test]
async fn two_pushes_in_one_second_keep_two_backups() {
    let config_path = scratch_config_path("record");
    let record = online_record_path(&config_path);
    write_online_record(&config_path, STARTUP);
    let (coordinator, _handler, _live) = coordinator_at(STARTUP, config_path.clone()).await;

    assert_eq!(
        coordinator.apply_reload(HOT_CHANGES).await,
        ReloadOutcome::Applied { generation: 1 }
    );
    assert_eq!(
        coordinator.apply_reload(RESTART_ONLY).await,
        ReloadOutcome::Applied { generation: 2 }
    );

    let backups = online_backups(&record);
    assert_eq!(
        backups.len(),
        2,
        "one stamp per push: a burst folded at the reload level still leaves two generations"
    );
    assert_eq!(
        std::fs::read_to_string(&record).expect("the record was written"),
        RESTART_ONLY
    );
    assert_eq!(
        std::fs::read_to_string(&backups[1]).expect("the newer backup was written"),
        HOT_CHANGES,
        "the newest backup is the one this push replaced"
    );
}

#[tokio::test]
async fn only_the_ten_newest_backups_are_kept() {
    let config_path = scratch_config_path("record");
    let record = online_record_path(&config_path);
    write_online_record(&config_path, STARTUP);
    let (coordinator, _handler, _live) = coordinator_at(STARTUP, config_path).await;

    // Twelve pushes that differ only in a restart-required field: each is new
    // text, so each is applied and each replaces the record.
    for generation in 1..=12u64 {
        let push = STARTUP.replace("tcp_timeout = 10", &format!("tcp_timeout = {generation}"));
        assert_eq!(
            coordinator.apply_reload(&push).await,
            ReloadOutcome::Applied { generation },
            "the push differs from the one in force, so it is applied"
        );
    }

    assert_eq!(
        online_backups(&record).len(),
        10,
        "the record keeps a window of versions, not every version it ever had"
    );
}

#[tokio::test]
async fn a_rejected_or_skipped_push_leaves_the_record_alone() {
    let config_path = scratch_config_path("record");
    let record = online_record_path(&config_path);
    write_online_record(&config_path, STARTUP);
    let (coordinator, _handler, _live) = coordinator_at(STARTUP, config_path.clone()).await;

    assert_eq!(
        coordinator.apply_reload("this = = not toml").await,
        ReloadOutcome::Rejected
    );
    assert_eq!(
        coordinator.apply_reload(STARTUP).await,
        ReloadOutcome::Skipped
    );

    assert_eq!(
        std::fs::read_to_string(&record).expect("the record was written"),
        STARTUP,
        "nothing came into force, so nothing was recorded"
    );
    assert!(
        online_backups(&record).is_empty(),
        "and the versions it did not replace stay single"
    );
}

#[tokio::test]
async fn an_unwritable_record_leaves_the_reload_in_force() {
    // A directory that does not exist: the record cannot be written, and the
    // reload it describes has already happened.
    let config_path = PathBuf::from("/unused/in/these/tests.toml");
    let (coordinator, handler, _live) = coordinator_at(STARTUP, config_path.clone()).await;

    assert_eq!(
        coordinator.apply_reload(HOT_CHANGES).await,
        ReloadOutcome::Applied { generation: 1 }
    );
    assert!(
        !handler.rotation_enabled(),
        "the swap stands even though its record failed"
    );
    assert!(
        !online_record_path(&config_path).exists(),
        "and the failed write left no file behind"
    );
}
