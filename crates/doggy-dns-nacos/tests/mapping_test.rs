#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::mapping::{service_name_from_name, to_dns_records};
use doggy_dns_nacos::test_support::{instances, qname};
use hickory_proto::rr::{LowerName, Name, Record, RecordType};
use std::str::FromStr;

/// The pair one plugin - and therefore one catalog - serves. Configured with the
/// Nacos spelling, queried with case-folded DNS labels.
const GROUP: &str = "DEFAULT_GROUP";
const NAMESPACE: &str = "public";

fn zone() -> Name {
    Name::from_str("nacos.local").unwrap()
}

/// The service name a name maps to under the configured pair.
fn service_of(text: &str) -> Option<String> {
    service_name_from_name(&qname(text), &zone(), GROUP, NAMESPACE).map(String::from)
}

#[test]
fn service_name_from_valid_dns_name() {
    assert_eq!(
        service_of("user-service.DEFAULT_GROUP.public.nacos.local."),
        Some("user-service".to_string())
    );
}

#[test]
fn service_name_from_dotted_service_name() {
    assert_eq!(
        service_of("com.example.api.DEFAULT_GROUP.public.nacos.local."),
        Some("com.example.api".to_string()),
        "dots inside the service name are not boundaries"
    );
}

#[test]
fn service_name_needs_three_labels_above_the_zone() {
    assert!(service_of("user-service.nacos.local.").is_none());
    assert!(service_of("a.b.nacos.local.").is_none());
}

#[test]
fn service_name_does_not_check_zone_membership() {
    // Membership is the caller's job. This name is a *sibling* of the zone, yet
    // the parser only counts labels from the right, so it happily returns the
    // service name a legitimate in-zone name would. That aliasing is exactly why
    // `NacosAuthority::lookup` must call `zone_of` first; `skips_sibling_zone_
    // with_shared_suffix` in `authority_test.rs` is the guard for that order.
    let sibling = qname("user-service.default_group.public.evilnacos.local.");
    assert_eq!(
        service_name_from_name(&sibling, &zone(), GROUP, NAMESPACE).map(String::from),
        Some("user-service".to_string())
    );
}

/// The group and namespace labels are matched, not carried: a catalog belongs to
/// one pair, so a name below another is nobody's to answer, and the service name
/// alone is enough to look up.
#[test]
fn service_name_requires_the_configured_group() {
    assert_eq!(
        service_of("user-service.DEFAULT_GROUP.public.nacos.local."),
        Some("user-service".to_string()),
        "the configured spelling matches the case-folded label"
    );
    assert!(service_of("user-service.OTHER_GROUP.public.nacos.local.").is_none());
}

#[test]
fn service_name_requires_the_configured_namespace() {
    assert!(service_of("user-service.DEFAULT_GROUP.other-ns.nacos.local.").is_none());
    // Swapping the two labels is not a match either way.
    assert!(service_of("user-service.public.DEFAULT_GROUP.nacos.local.").is_none());
}

#[test]
fn service_name_rejects_a_label_carrying_a_control_byte() {
    // No Nacos service, group or namespace is named with a control byte, and this
    // is untrusted network input heading for the SDK and the logs. The label must
    // be supplied as raw bytes: `&str` labels go through IDNA/UTS46 validation,
    // which rejects or rewrites a control byte, so this is the only way to get
    // 0x01 onto a name in a test.
    let forged = Name::from_labels([
        b"a\x01b".as_slice(),
        b"default_group".as_slice(),
        b"public".as_slice(),
        b"nacos".as_slice(),
        b"local".as_slice(),
    ])
    .expect("raw labels are only length-checked (1..=63 bytes)");
    assert!(service_name_from_name(&LowerName::from(forged), &zone(), GROUP, NAMESPACE).is_none());
}

#[test]
fn service_name_rejects_a_dot_inside_one_label() {
    // `com\.example\.api` is one label whose text contains dots, and it must
    // not alias `com.example.api` (three labels), the canonical spelling of a
    // dotted service. The old string parser split the presentation form at the
    // dots and produced a name that could never match, so the authority answered
    // NXDOMAIN; rejecting the label keeps that answer.
    assert!(service_of("com\\.example\\.api.DEFAULT_GROUP.public.nacos.local.").is_none());
}

/// Every record must carry the name it was asked for, case-folded.
///
/// The wire bytes of a nacos hit already pin this for the one-name case
/// (`wire_golden_test.rs`), but the authority stops handing one owned `Name` to
/// the builder, so the per-record name needs its own assertion: a record built
/// with the wrong owner would still serialize, still match the query type, and
/// still be counted as an answer.
fn assert_records_named(records: &[Record], name: &LowerName) {
    let expected = Name::from(name.clone());
    for record in records {
        assert_eq!(
            &record.name, &expected,
            "record {record} carries a name other than the queried one"
        );
    }
}

#[test]
fn to_dns_records_creates_a_records() {
    let name = qname("user-service.DEFAULT_GROUP.public.nacos.local.");
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "10.0.1.6"]),
        6,
        RecordType::A,
    );

    assert_eq!(records.len(), 2);
    assert_records_named(&records, &name);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl, 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let name = qname("svc.DEFAULT_GROUP.public.nacos.local.");
    let records = to_dns_records(&name, &instances(&["::1"]), 6, RecordType::AAAA);

    assert_eq!(records.len(), 1);
    assert_records_named(&records, &name);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_filters_a_records_for_a_query() {
    let name = qname("svc.DEFAULT_GROUP.public.nacos.local.");
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::A,
    );

    assert_eq!(records.len(), 1);
    assert_records_named(&records, &name);
    assert_eq!(records[0].record_type(), RecordType::A);
}

#[test]
fn to_dns_records_filters_aaaa_records_for_aaaa_query() {
    let name = qname("svc.DEFAULT_GROUP.public.nacos.local.");
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::AAAA,
    );

    assert_eq!(records.len(), 1);
    assert_records_named(&records, &name);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

/// Answer order is the cache order; the record move in `take_answer_records`
/// depends on it, so pin it down before anything can reorder.
#[test]
fn to_dns_records_preserves_instance_order() {
    let name = qname("svc.DEFAULT_GROUP.public.nacos.local.");
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "10.0.1.6", "10.0.1.7"]),
        6,
        RecordType::A,
    );

    assert_records_named(&records, &name);
    let addresses: Vec<String> = records
        .iter()
        .map(|record| record.data.to_string())
        .collect();
    assert_eq!(addresses, ["10.0.1.5", "10.0.1.6", "10.0.1.7"]);
}
