//! Unit tests for the `AYA_LOG_LEVEL` parser.
//!
//! The parsed value is baked into the eBPF program's `AYA_LOG_LEVEL` global
//! before load, so these tests pin the rank mapping (off < error < warn) and
//! the clamping rules independently of any kernel-side build.

use doggy_dns_xdp::kernel_log::{KernelLevel, resolve_level};

#[test]
fn parses_supported_levels() {
    assert_eq!(KernelLevel::parse("off"), Some(KernelLevel::Off));
    assert_eq!(KernelLevel::parse("error"), Some(KernelLevel::Error));
    assert_eq!(KernelLevel::parse("warn"), Some(KernelLevel::Warn));
}

#[test]
fn trims_and_ignores_case() {
    assert_eq!(KernelLevel::parse("  WARN\n"), Some(KernelLevel::Warn));
    assert_eq!(KernelLevel::parse("Error"), Some(KernelLevel::Error));
}

#[test]
fn unset_defaults_to_warn_without_complaint() {
    assert_eq!(resolve_level(None), (KernelLevel::Warn, true));
}

#[test]
fn unsupported_values_clamp_to_warn() {
    for raw in ["info", "debug", "trace", "verbose", ""] {
        assert_eq!(
            resolve_level(Some(raw)),
            (KernelLevel::Warn, false),
            "{raw:?} must clamp to warn and be reported as unsupported"
        );
    }
}

#[test]
fn supported_values_are_reported_as_such() {
    assert_eq!(resolve_level(Some("off")), (KernelLevel::Off, true));
    assert_eq!(resolve_level(Some("error")), (KernelLevel::Error, true));
    assert_eq!(resolve_level(Some(" Warn ")), (KernelLevel::Warn, true));
}

#[test]
fn ranks_match_the_kernel_threshold_order() {
    assert_eq!(KernelLevel::Off.as_u8(), 0);
    assert_eq!(KernelLevel::Error.as_u8(), 1);
    assert_eq!(KernelLevel::Warn.as_u8(), 2);
}
