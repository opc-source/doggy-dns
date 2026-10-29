//! Parser for the `AYA_LOG_LEVEL` value baked into the eBPF program.
//!
//! Kernel-side filtering compares the log site's level against this value, so
//! the ranks must match `aya-log-common`'s `Level` enum: `Error = 1`,
//! `Warn = 2`, ... and zero disables logging entirely. Only the levels the
//! program actually uses are accepted; verbose values are clamped to `Warn`
//! because there are no `info`/`debug`/`trace` call sites to enable.

/// Kernel-side verbosity. Ranks follow `aya-log-common`'s `Level` ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelLevel {
    /// Disable every log site.
    Off,
    /// Only records from failure paths that drop the packet.
    Error,
    /// The default: drops plus abnormal packets that are passed on.
    Warn,
}

impl KernelLevel {
    /// Parse one `AYA_LOG_LEVEL` value, case-insensitively and trimmed.
    ///
    /// Returns `None` for levels with no kernel-side call sites (`info`,
    /// `debug`, `trace`) as well as for garbage: both are unsupported here.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            _ => None,
        }
    }

    /// The value written to the program's `AYA_LOG_LEVEL` global.
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Error => 1,
            Self::Warn => 2,
        }
    }
}

/// Resolve an optional `AYA_LOG_LEVEL` value for loading the eBPF program.
///
/// The second element reports whether the value is supported: `None` (the
/// variable is unset) takes the default and counts as supported, while an
/// unsupported or malformed value clamps to [`KernelLevel::Warn`] and returns
/// `false` so the caller can surface the typo.
pub fn resolve_level(raw: Option<&str>) -> (KernelLevel, bool) {
    match raw {
        None => (KernelLevel::Warn, true),
        Some(raw) => match KernelLevel::parse(raw) {
            Some(level) => (level, true),
            None => (KernelLevel::Warn, false),
        },
    }
}
