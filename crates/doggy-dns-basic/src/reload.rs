//! Rules deciding whether a config change can be applied to a running
//! process, or needs a restart.
//!
//! Hot reload rebuilds the query path only — the authority chain built from
//! `[[plugins]]`, the answer-rotation flag and the query-logging toggle.
//! Everything else is wired in before that path can be swapped: sockets and
//! TLS listeners are bound once by `build_server`, the tokio runtime (worker
//! count, CPU pinning) exists before the config is ever consumed a second
//! time, the tracing subscriber is process-global and installed once, the XDP
//! program is attached at start-up, and the remote-config source is what
//! delivers the change in the first place.
//!
//! The rule is therefore an allow-list: `[[plugins]]`, `server.rotate_answers`
//! and `middleware.logging` may differ; anything else that differs makes the
//! whole reload invalid, so an operator gets one clear "restart required"
//! message instead of a config half-applied.

use crate::DoggyDnsConfig;

/// Config paths whose change forbids an in-place reload.
///
/// An empty list means the candidate may be applied to the running process.
/// Paths are reported as they would be written in the TOML file
/// (`server.port`, `observability`, …) so the message names the line the
/// operator actually changed.
pub fn reload_conflicts(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> Vec<String> {
    let mut conflicts = Vec::new();

    // Server: `rotate_answers` is the one query-path field and stays out of
    // the comparison; the sockets, timeouts and runtime shape are all fixed
    // before the first query is served.
    compare(
        &mut conflicts,
        "server.listen_addr",
        &old.server.listen_addr,
        &new.server.listen_addr,
    );
    compare(
        &mut conflicts,
        "server.port",
        &old.server.port,
        &new.server.port,
    );
    compare(
        &mut conflicts,
        "server.tcp_timeout",
        &old.server.tcp_timeout,
        &new.server.tcp_timeout,
    );
    compare(
        &mut conflicts,
        "server.shutdown_timeout",
        &old.server.shutdown_timeout,
        &new.server.shutdown_timeout,
    );
    compare(
        &mut conflicts,
        "server.worker_threads",
        &old.server.worker_threads,
        &new.server.worker_threads,
    );
    compare(
        &mut conflicts,
        "server.reuseport_listen_num",
        &old.server.reuseport_listen_num,
        &new.server.reuseport_listen_num,
    );
    compare(
        &mut conflicts,
        "server.cpu_affinity",
        &old.server.cpu_affinity,
        &new.server.cpu_affinity,
    );
    compare(
        &mut conflicts,
        "server.tls",
        &old.server.tls,
        &new.server.tls,
    );
    compare(
        &mut conflicts,
        "server.https",
        &old.server.https,
        &new.server.https,
    );

    // The log format is baked into the tracing subscriber at start-up.
    compare(
        &mut conflicts,
        "middleware.log_format",
        &old.middleware.log_format,
        &new.middleware.log_format,
    );

    // Sections whose entire configuration is consumed once, at start-up.
    compare(
        &mut conflicts,
        "remote_config",
        &old.remote_config,
        &new.remote_config,
    );
    compare(
        &mut conflicts,
        "observability",
        &old.observability,
        &new.observability,
    );
    compare(&mut conflicts, "xdp", &old.xdp, &new.xdp);
    compare(&mut conflicts, "control", &old.control, &new.control);

    conflicts
}

/// Whether every difference between the two configs is reloadable.
pub fn is_reloadable(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> bool {
    reload_conflicts(old, new).is_empty()
}

fn compare<T: PartialEq>(conflicts: &mut Vec<String>, path: &str, old: &T, new: &T) {
    if old != new {
        conflicts.push(path.to_string());
    }
}
