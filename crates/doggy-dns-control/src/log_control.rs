//! Runtime log-filter control, exposed through the MCP `set_log_level` tool.

use serde::Serialize;
use std::fmt;

/// The directives in force on each log output; `None` means that output is off.
/// Returned by `set_log_level` and nested under `logs` in `get_runtime_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLevelState {
    pub stdout: Option<String>,
    pub otlp: Option<String>,
}

/// Live log-filter control, implemented by the binary over the observability
/// layer's subscriber handle.
pub trait LogLevelAdmin: Send + Sync + fmt::Debug {
    fn current(&self) -> LogLevelState;
    /// Replace the directives on every enabled output; nothing changes when
    /// the directives do not parse.
    fn set(&self, directives: &str) -> Result<LogLevelState, String>;
    /// Restore each output to the directives it started with.
    fn reset(&self) -> Result<LogLevelState, String>;
}
