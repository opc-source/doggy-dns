#![allow(clippy::disallowed_methods)]

//! The reload orchestrator: what it accepts, what it refuses, what it leaves
//! untouched when it refuses.
//!
//! `ReloadOrchestrator::apply` is the single decision point every trigger
//! funnel into, so these tests drive it directly over configs whose chains
//! build without any network (forward authorities only resolve when queried).
//! The swap itself is covered by `crates/doggy-dns-core/tests/handler_reload_test.rs`
//! and end to end by `tests/nacos_e2e_test.rs`.

use doggy_dns::reload::{
    ReloadOrchestrator, ReloadOutcome, ReloadRequest, build_chain, run_reload_loop,
};
use doggy_dns_core::config::parse_config;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::middleware::logging::LoggingToggle;
use doggy_dns_nacos::pool::PooledWatcher;
use std::path::PathBuf;
use std::sync::Arc;

const PORT: u16 = 15499;

/// A valid config with the given middleware flag and plugin block.
fn config(logging: bool, plugins: &str) -> String {
    format!(
        r#"
[server]
listen_addr = "127.0.0.1"
port = {PORT}
rotate_answers = true

[middleware]
logging = {logging}

{plugins}
"#
    )
}

const ONE_PLUGIN: &str = r#"
[[plugins]]
kind = "forward"
enabled = true
upstream = ["192.0.2.1:53"]
"#;

const TWO_PLUGINS: &str = r#"
[[plugins]]
kind = "forward"
enabled = true
upstream = ["192.0.2.1:53"]

[[plugins]]
kind = "forward"
enabled = true
upstream = ["192.0.2.2:53"]
"#;

fn temp_path(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("reload-orchestrator-{label}.toml"))
}

/// Start a process-shaped pair: a handler answering from `content` plus the
/// orchestrator that may replace what it answers from.
async fn setup(label: &str, content: &str) -> (ReloadOrchestrator, LoggingToggle, PathBuf) {
    let config = parse_config(content).expect("the test config must be valid");
    let mut pool: Vec<PooledWatcher> = Vec::new();
    let chain = build_chain(&config, &mut pool)
        .await
        .expect("the test chain must build");
    let handler = DoggyDnsHandler::new(
        vec![],
        Arc::new(chain),
        false,
        AnswerRotation::new(config.server.rotate_answers),
    );

    let toggle = LoggingToggle::new(config.middleware.logging);
    let path = temp_path(label);
    std::fs::write(&path, content).expect("the test config must be written");

    let orchestrator = ReloadOrchestrator::new(
        path.display().to_string(),
        config,
        pool,
        handler.reload_handle(),
        toggle.clone(),
    );
    (orchestrator, toggle, path)
}

#[tokio::test]
async fn sighup_applies_a_query_path_change() {
    let (orchestrator, toggle, path) = setup("sighup", &config(true, ONE_PLUGIN)).await;
    assert!(toggle.is_enabled());

    std::fs::write(&path, config(false, ONE_PLUGIN)).expect("the new config must be written");
    let outcome = orchestrator.apply(ReloadRequest::Sighup).await;

    assert_eq!(
        outcome,
        ReloadOutcome::Applied {
            generation: 1,
            authorities: 1
        }
    );
    assert!(
        !toggle.is_enabled(),
        "a reloaded middleware.logging must reach the live middleware"
    );
    assert_eq!(orchestrator.generation().await, 1);
}

#[tokio::test]
async fn a_pushed_config_becomes_authoritative_without_touching_the_file() {
    // The remote source is the point of remote config: what is in force is what
    // was pushed, and the file on disk keeps the previous content until a
    // restart re-reads it.
    let on_disk = config(true, ONE_PLUGIN);
    let (orchestrator, toggle, path) = setup("push", &on_disk).await;

    let outcome = orchestrator
        .apply(ReloadRequest::Nacos {
            content: config(false, TWO_PLUGINS),
        })
        .await;

    assert_eq!(
        outcome,
        ReloadOutcome::Applied {
            generation: 1,
            authorities: 2
        }
    );
    assert!(!toggle.is_enabled());
    assert_eq!(orchestrator.current_config().await.plugins.len(), 2);
    assert_eq!(
        std::fs::read_to_string(&path).expect("config file"),
        on_disk,
        "a push must not rewrite the local file"
    );
}

#[tokio::test]
async fn a_forbidden_change_is_rejected_and_nothing_moves() {
    let (orchestrator, toggle, path) = setup("reject", &config(true, ONE_PLUGIN)).await;

    std::fs::write(
        &path,
        config(true, TWO_PLUGINS).replace(&PORT.to_string(), "15500"),
    )
    .expect("the new config must be written");
    let outcome = orchestrator.apply(ReloadRequest::Sighup).await;

    assert_eq!(
        outcome,
        ReloadOutcome::Rejected {
            conflicts: vec!["server.port".to_string()]
        },
        "a reload that would half-apply must be refused whole"
    );
    assert_eq!(orchestrator.generation().await, 0);
    assert!(
        toggle.is_enabled(),
        "a rejected candidate must not have been applied"
    );
    assert_eq!(
        orchestrator.current_config().await.plugins.len(),
        1,
        "the rejected plugin change must not have been applied either"
    );
}

#[tokio::test]
async fn an_unreadable_config_fails_without_moving_the_generation() {
    let (orchestrator, _toggle, path) = setup("unreadable", &config(true, ONE_PLUGIN)).await;
    std::fs::write(&path, "this is not toml [[[").expect("the broken config must be written");

    let outcome = orchestrator.apply(ReloadRequest::Sighup).await;

    let ReloadOutcome::Failed { error } = outcome else {
        panic!("unparseable content must fail, got {outcome:?}");
    };
    assert!(
        error.contains("failed to load config from"),
        "the failure must name where the config came from: {error}"
    );
    assert_eq!(orchestrator.generation().await, 0);
}

#[tokio::test]
async fn a_missing_config_file_fails_without_killing_the_query_path() {
    let (orchestrator, toggle, path) = setup("missing-file", &config(true, ONE_PLUGIN)).await;
    std::fs::remove_file(&path).expect("the config file must be removable");

    let outcome = orchestrator.apply(ReloadRequest::Sighup).await;
    assert!(
        matches!(outcome, ReloadOutcome::Failed { .. }),
        "a vanished config file must fail the reload, got {outcome:?}"
    );

    // The process is still reloadable: put a valid config back and it applies.
    std::fs::write(&path, config(false, ONE_PLUGIN)).expect("the config must be restorable");
    assert!(matches!(
        orchestrator.apply(ReloadRequest::Sighup).await,
        ReloadOutcome::Applied { generation: 1, .. }
    ));
    assert!(!toggle.is_enabled());
}

#[tokio::test]
async fn generations_count_applied_reloads_only() {
    let (orchestrator, _toggle, path) = setup("generation", &config(true, ONE_PLUGIN)).await;

    assert!(matches!(
        orchestrator.apply(ReloadRequest::Sighup).await,
        ReloadOutcome::Applied { generation: 1, .. }
    ));

    // Rejected, then failed: neither advances the counter, so the number keeps
    // meaning "how many query paths this process has served".
    std::fs::write(
        &path,
        config(true, ONE_PLUGIN).replace(&PORT.to_string(), "15500"),
    )
    .expect("write");
    assert!(matches!(
        orchestrator.apply(ReloadRequest::Sighup).await,
        ReloadOutcome::Rejected { .. }
    ));
    assert!(matches!(
        orchestrator
            .apply(ReloadRequest::Nacos {
                content: "not toml [[[".to_string(),
            })
            .await,
        ReloadOutcome::Failed { .. }
    ));
    assert_eq!(orchestrator.generation().await, 1);

    std::fs::write(&path, config(true, TWO_PLUGINS)).expect("write");
    assert!(matches!(
        orchestrator.apply(ReloadRequest::Sighup).await,
        ReloadOutcome::Applied { generation: 2, .. }
    ));
}

#[tokio::test]
async fn a_burst_collapses_into_the_newest_request() {
    // Both requests are queued before the loop starts, so they land in one
    // debounce window. Applying them one at a time instead would have installed
    // the first candidate and flipped the toggle; newest-wins means the burst
    // is decided by the invalid request and nothing is applied.
    let (orchestrator, toggle, _path) = setup("burst", &config(true, ONE_PLUGIN)).await;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    sender
        .send(ReloadRequest::Nacos {
            content: config(false, ONE_PLUGIN),
        })
        .expect("send");
    sender
        .send(ReloadRequest::Nacos {
            content: "not toml [[[".to_string(),
        })
        .expect("send");
    drop(sender);

    run_reload_loop(orchestrator, receiver).await;

    assert!(
        toggle.is_enabled(),
        "the coalesced burst must be decided by its newest request, not applied in order"
    );
}

#[tokio::test]
async fn a_burst_of_rejected_requests_is_decided_by_the_newest() {
    // Same shape as above, with a rejectable (valid but unsafe) newest request:
    // one reload is attempted, and it is the refused one.
    let (orchestrator, toggle, _path) = setup("burst-rejected", &config(true, ONE_PLUGIN)).await;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    sender
        .send(ReloadRequest::Nacos {
            content: config(false, ONE_PLUGIN),
        })
        .expect("send");
    sender
        .send(ReloadRequest::Nacos {
            content: config(true, TWO_PLUGINS).replace(&PORT.to_string(), "15500"),
        })
        .expect("send");
    drop(sender);

    run_reload_loop(orchestrator, receiver).await;

    assert!(
        toggle.is_enabled(),
        "the newest request was rejected, so the burst must not have applied the older one"
    );
}

#[tokio::test]
async fn concurrent_requests_are_applied_one_at_a_time() {
    // A SIGHUP and a push racing each other must serialise: each applies
    // against the config the previous one installed, so generations stay dense.
    let (orchestrator, _toggle, path) = setup("concurrent", &config(true, ONE_PLUGIN)).await;
    std::fs::write(&path, config(true, TWO_PLUGINS)).expect("write");
    let orchestrator = Arc::new(orchestrator);

    let first = {
        let orchestrator = Arc::clone(&orchestrator);
        tokio::spawn(async move { orchestrator.apply(ReloadRequest::Sighup).await })
    };
    let second = {
        let orchestrator = Arc::clone(&orchestrator);
        tokio::spawn(async move {
            orchestrator
                .apply(ReloadRequest::Nacos {
                    content: config(true, ONE_PLUGIN),
                })
                .await
        })
    };

    let first = first.await.expect("reload task panicked");
    let second = second.await.expect("reload task panicked");
    assert!(
        matches!(first, ReloadOutcome::Applied { .. }),
        "got {first:?}"
    );
    assert!(
        matches!(second, ReloadOutcome::Applied { .. }),
        "got {second:?}"
    );

    let generations = [outcome_generation(&first), outcome_generation(&second)];
    assert_eq!(
        generations.iter().copied().max().unwrap_or_default(),
        2,
        "two applied reloads must reach generation 2, got {generations:?}"
    );
    assert_eq!(orchestrator.generation().await, 2);
}

fn outcome_generation(outcome: &ReloadOutcome) -> u64 {
    match outcome {
        ReloadOutcome::Applied { generation, .. } => *generation,
        ReloadOutcome::Rejected { .. } | ReloadOutcome::Failed { .. } => 0,
    }
}
