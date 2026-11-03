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
//! The rule is an allow-list, and it is enforced structurally: both configs are
//! serialised and walked as TOML, and every differing path outside [`HOT_PATHS`]
//! is reported. A field someone adds to a config struct is therefore restart
//! required the moment it exists, with no list to remember to update — the
//! failure mode of a hand-written comparison is that a field nobody added to
//! either branch is simply never compared, and a reload that cannot honour it
//! reports success.
//!
//! Two consequences of comparing serialised values instead of Rust structs:
//!
//! - A config type must serialise everything it can be configured to.
//!   `skip_serializing_if` on a config field makes two different configs look
//!   identical here and silently permits their reload.
//! - The reported paths are as fine as the TOML document: changing
//!   `[server.tls].port` reports `server.tls.port`, not `server.tls`. That is
//!   the better message anyway — it names the line the operator edited.

use crate::DoggyDnsConfig;

/// The config paths a reload of the query path can carry.
///
/// Anything differing at or below one of these is reloadable; everything else
/// differing is a restart. `plugins` is here because the chain is rebuilt from
/// it, including which plugins are enabled and in what order.
pub const HOT_PATHS: &[&str] = &["plugins", "middleware.logging", "server.rotate_answers"];

/// The conflict reported when the two configs cannot be compared as TOML.
///
/// Deliberately not an empty list: this check is the gate, so an incomparable
/// pair has to be refused rather than waved through.
const INCOMPARABLE: &str = "config (not comparable as TOML)";

/// Config paths whose change forbids an in-place reload.
///
/// An empty list means the candidate may be applied to the running process.
/// Paths are reported as they would be written in the TOML file
/// (`server.port`, `observability.endpoint`, …).
pub fn reload_conflicts(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> Vec<String> {
    let (Ok(old), Ok(new)) = (toml::Value::try_from(old), toml::Value::try_from(new)) else {
        return vec![INCOMPARABLE.to_string()];
    };
    let mut conflicts = diff_paths(&old, &new);
    conflicts.retain(|path| !is_hot_path(path));
    conflicts
}

/// Every path at which two TOML values differ, hottest filter aside.
///
/// A table is walked key by key so the report stops at the deepest thing that
/// actually changed; every other value, arrays included, is compared whole.
pub fn diff_paths(old: &toml::Value, new: &toml::Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_diffs("", old, new, &mut paths);
    paths
}

/// Whether every difference between the two configs is reloadable.
pub fn is_reloadable(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> bool {
    reload_conflicts(old, new).is_empty()
}

/// Whether `path` is the hot subset or something under it.
fn is_hot_path(path: &str) -> bool {
    !path.is_empty()
        && HOT_PATHS
            .iter()
            .any(|hot| path == *hot || path.starts_with(&format!("{hot}.")))
}

fn collect_diffs(prefix: &str, old: &toml::Value, new: &toml::Value, out: &mut Vec<String>) {
    if let (toml::Value::Table(old), toml::Value::Table(new)) = (old, new) {
        // A key present in both tables would otherwise be walked twice.
        let keys: std::collections::BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        for key in keys {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match (old.get(key), new.get(key)) {
                (Some(x), Some(y)) => collect_diffs(&path, x, y, out),
                // Present on one side only: a section added or dropped by the
                // candidate is a change the running process cannot make.
                _ => out.push(path),
            }
        }
        return;
    }
    if old != new && !prefix.is_empty() {
        out.push(prefix.to_string());
    }
}
