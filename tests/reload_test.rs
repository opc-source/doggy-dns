#![allow(clippy::disallowed_methods)]

//! The reload pipeline as the two triggers see it: one entry point, the same
//! guarantees whichever source arrives. These drive the real coordinator, so
//! what a rejected push leaves behind is the production code path, not a mock.

use doggy_dns::reload::{
    Applied, PluginSlot, ReloadCoordinator, ReloadOutcome, ReloadSource, authority_chain_from,
    build_chain, middlewares_for, nacos_zone_signals, restart_required,
};
use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::{DoggyDnsConfig, PluginKind, parse_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_plugin::Middleware;
use std::path::PathBuf;
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

/// The startup sequence from `main.rs`: build the chain, hand it to the
/// handler, adopt both in the coordinator.
async fn coordinator_for(
    content: &str,
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
        PathBuf::from("/unused/in/these/tests.toml"),
        Applied {
            raw: content.to_string(),
            config,
            slots,
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

// --- the coordinator ---

#[tokio::test]
async fn hot_fields_are_applied_without_a_restart() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;
    assert!(handler.rotation_enabled());
    assert_eq!(handler.middlewares().len(), 1, "logging is on at startup");

    let outcome = coordinator
        .apply_reload(ReloadSource::Nacos, Some(HOT_CHANGES.to_string()))
        .await;

    assert_eq!(outcome, ReloadOutcome::Applied);
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

    let outcome = coordinator
        .apply_reload(ReloadSource::Sighup, Some("this = = not toml".to_string()))
        .await;

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

    let outcome = coordinator
        .apply_reload(ReloadSource::Nacos, Some(broken))
        .await;

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

    let outcome = coordinator
        .apply_reload(ReloadSource::Nacos, Some(STARTUP.to_string()))
        .await;

    assert_eq!(
        outcome,
        ReloadOutcome::Skipped,
        "re-pushing the content already in force must not rebuild the chain"
    );
}

#[tokio::test]
async fn unreadable_file_is_rejected() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;

    let outcome = coordinator.apply_reload(ReloadSource::Sighup, None).await;

    assert_eq!(outcome, ReloadOutcome::Rejected);
    assert!(handler.rotation_enabled());
}

#[tokio::test]
async fn restart_required_only_diff_is_recorded_and_reported() {
    let (coordinator, handler, _live) = coordinator_for(STARTUP).await;

    let outcome = coordinator
        .apply_reload(ReloadSource::Nacos, Some(RESTART_ONLY.to_string()))
        .await;

    // The reload succeeds: the hot set is unchanged, but the content is
    // recorded so the same push is not re-diffed every time.
    assert_eq!(outcome, ReloadOutcome::Applied);
    assert!(handler.rotation_enabled());
    assert_eq!(
        coordinator.applied_config().await.server.tcp_timeout,
        42,
        "the recorded config shows the pushed value while the running listener still uses 10"
    );
    assert_eq!(
        restart_required(&config_of(STARTUP), &config_of(RESTART_ONLY)),
        vec!["server.tcp_timeout".to_string()]
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
