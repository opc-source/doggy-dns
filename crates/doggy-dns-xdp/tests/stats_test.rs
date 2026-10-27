#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::stats::XdpStatsDelta;
use doggy_dns_xdp::types::XdpStats;

#[test]
fn layout_matches_the_ebpf_program() {
    assert_eq!(core::mem::size_of::<XdpStats>(), 40);
    assert_eq!(core::mem::align_of::<XdpStats>(), 8);
}

/// Ordered `pub <name>:` field names of the `pub struct XdpStats {` body,
/// up to its closing brace. Returns `None` when the struct is not found.
fn xdp_stats_field_order(source: &str) -> Option<Vec<&str>> {
    let marker = "pub struct XdpStats {";
    let start = source.find(marker)?;
    let mut fields = Vec::new();
    for line in source[start + marker.len()..].lines() {
        let line = line.trim();
        if line.starts_with('}') {
            return Some(fields);
        }
        if let Some(rest) = line.strip_prefix("pub ") {
            fields.push(rest.split(':').next()?.trim());
        }
    }
    None
}

/// Ordered field names, failing if the struct is missing or has no fields.
fn field_order_from_source<'a>(source: &'a str, path: &str) -> Vec<&'a str> {
    let fields = xdp_stats_field_order(source)
        .unwrap_or_else(|| panic!("no `pub struct XdpStats {{` found in {path}"));
    assert!(!fields.is_empty(), "empty XdpStats field list in {path}");
    fields
}

#[test]
fn xdp_stats_field_order_matches_across_workspaces() {
    // XdpStats is duplicated across the two cargo workspaces; BPF map values
    // are raw byte layouts, so a reordered field would silently relabel the
    // kernel's counters while the size asserts still pass. Compare the field
    // order as text — this runs on any platform, no eBPF toolchain needed.
    let userspace_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/types.rs");
    let kernel_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../xdp/doggy-dns-xdp-ebpf/src/main.rs"
    );

    let userspace_source = std::fs::read_to_string(userspace_path)
        .unwrap_or_else(|error| panic!("failed to read {userspace_path}: {error}"));
    let kernel_source = std::fs::read_to_string(kernel_path)
        .unwrap_or_else(|error| panic!("failed to read {kernel_path}: {error}"));

    let userspace = field_order_from_source(&userspace_source, userspace_path);
    let kernel = field_order_from_source(&kernel_source, kernel_path);
    let expected = ["hit", "negative_hit", "miss", "expired", "drop"];

    assert_eq!(
        userspace, expected,
        "field order drifted in {userspace_path}"
    );
    assert_eq!(kernel, expected, "field order drifted in {kernel_path}");
    assert_eq!(
        userspace, kernel,
        "XdpStats field order differs between {userspace_path} and {kernel_path}"
    );
}

#[test]
fn delta_of_the_first_snapshot_is_the_snapshot() {
    let current = XdpStats {
        hit: 3,
        negative_hit: 1,
        miss: 2,
        expired: 4,
        drop: 5,
    };

    let delta = current.delta_from(&XdpStats::default());

    assert_eq!(delta.hit, 3);
    assert_eq!(delta.negative_hit, 1);
    assert_eq!(delta.miss, 2);
    assert_eq!(delta.expired, 4);
    assert_eq!(delta.drop, 5);
    assert!(!delta.reset);
}

#[test]
fn delta_subtracts_previous_counts() {
    let previous = XdpStats {
        hit: 10,
        negative_hit: 2,
        miss: 5,
        expired: 1,
        drop: 0,
    };
    let current = XdpStats {
        hit: 13,
        negative_hit: 2,
        miss: 8,
        expired: 1,
        drop: 1,
    };

    let delta = current.delta_from(&previous);

    assert_eq!(delta.hit, 3);
    assert_eq!(delta.negative_hit, 0);
    assert_eq!(delta.miss, 3);
    assert_eq!(delta.expired, 0);
    assert_eq!(delta.drop, 1);
    assert!(!delta.reset);
}

#[test]
fn delta_flags_reset_when_a_counter_goes_backwards() {
    let previous = XdpStats {
        hit: 10,
        ..Default::default()
    };
    let current = XdpStats {
        hit: 2,
        miss: 1,
        ..Default::default()
    };

    let delta = current.delta_from(&previous);

    assert!(delta.reset);
    assert_eq!(delta.hit, 2);
    assert_eq!(delta.miss, 1);
}

#[test]
fn entries_skip_zero_counters() {
    let delta = XdpStatsDelta {
        hit: 2,
        negative_hit: 0,
        miss: 1,
        expired: 0,
        drop: 0,
        reset: false,
    };

    let entries: Vec<_> = delta.entries().collect();

    assert_eq!(entries, vec![("hit", 2), ("miss", 1)]);
}

#[test]
fn emit_delta_reports_only_non_zero_changes() {
    use doggy_dns_xdp::stats::emit_delta;

    let mut previous = XdpStats::default();
    let mut recorded = Vec::new();

    emit_delta(
        &mut previous,
        XdpStats {
            hit: 1,
            ..Default::default()
        },
        &mut |label, count| recorded.push((label, count)),
    );

    assert_eq!(recorded, vec![("hit", 1)]);
    assert_eq!(previous.hit, 1);

    recorded.clear();
    emit_delta(
        &mut previous,
        XdpStats {
            hit: 3,
            miss: 2,
            ..Default::default()
        },
        &mut |label, count| recorded.push((label, count)),
    );

    assert_eq!(recorded, vec![("hit", 2), ("miss", 2)]);
    assert_eq!(previous.hit, 3);
}
