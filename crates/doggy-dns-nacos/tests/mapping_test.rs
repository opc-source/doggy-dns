#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::mapping::{service_key_from_name, to_dns_records};
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use hickory_proto::rr::{LowerName, Name, RecordType};
use std::str::FromStr;

fn zone() -> Name {
    Name::from_str("nacos.local").unwrap()
}

fn lower(name: &str) -> LowerName {
    LowerName::from(Name::from_str(name).unwrap())
}

#[test]
fn service_key_from_valid_dns_name() {
    let key = service_key_from_name(
        &lower("user-service.DEFAULT_GROUP.public.nacos.local."),
        &zone(),
    )
    .expect("three labels above the zone");
    assert_eq!(
        key,
        ServiceKey::new("user-service", "default_group", "public")
    );
}

#[test]
fn service_key_from_dotted_service_name() {
    let key = service_key_from_name(
        &lower("com.example.api.DEFAULT_GROUP.public.nacos.local."),
        &zone(),
    )
    .expect("dots inside the service name are not boundaries");
    assert_eq!(
        key,
        ServiceKey::new("com.example.api", "default_group", "public")
    );
}

#[test]
fn service_key_needs_three_labels_above_the_zone() {
    assert!(service_key_from_name(&lower("user-service.nacos.local."), &zone()).is_none());
    assert!(service_key_from_name(&lower("a.b.nacos.local."), &zone()).is_none());
}

#[test]
fn service_key_does_not_check_zone_membership() {
    // Membership is the caller's job. This name is a *sibling* of the zone, yet
    // the key builder only counts labels from the right, so it happily produces
    // the same key a legitimate in-zone name would. That aliasing is exactly why
    // `NacosAuthority::lookup` must call `zone_of` first; `skips_sibling_zone_
    // with_shared_suffix` in `authority_test.rs` is the guard for that order.
    let sibling = lower("user-service.default_group.public.evilnacos.local.");
    assert_eq!(
        service_key_from_name(&sibling, &zone()),
        Some(ServiceKey::new("user-service", "default_group", "public"))
    );
}

#[test]
fn service_key_rejects_a_label_carrying_the_separator() {
    // A 0x01 byte inside a label would otherwise forge a field boundary. The
    // label must be supplied as raw bytes: `&str` labels go through IDNA/UTS46
    // validation, which rejects or rewrites a control byte, so this is the only
    // way to get 0x01 onto a name in a test.
    let forged = Name::from_labels([
        b"a\x01b".as_slice(),
        b"default_group".as_slice(),
        b"public".as_slice(),
        b"nacos".as_slice(),
        b"local".as_slice(),
    ])
    .expect("raw labels are only length-checked (1..=63 bytes)");
    assert!(service_key_from_name(&LowerName::from(forged), &zone()).is_none());
}

fn instances(ips: &[&str]) -> Vec<CachedInstance> {
    ips.iter()
        .map(|ip| CachedInstance {
            ip: ip.parse().unwrap(),
            port: 8080,
            healthy: true,
        })
        .collect()
}

#[test]
fn to_dns_records_creates_a_records() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "10.0.1.6"]),
        6,
        RecordType::A,
    );

    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl, 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &instances(&["::1"]), 6, RecordType::AAAA);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_filters_a_records_for_a_query() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::A,
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::A);
}

#[test]
fn to_dns_records_filters_aaaa_records_for_aaaa_query() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::AAAA,
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

/// Answer order is the cache order; the record move in the next task depends on
/// it, so pin it down before anything can reorder.
#[test]
fn to_dns_records_preserves_instance_order() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "10.0.1.6", "10.0.1.7"]),
        6,
        RecordType::A,
    );

    let addresses: Vec<String> = records
        .iter()
        .map(|record| record.data.to_string())
        .collect();
    assert_eq!(addresses, ["10.0.1.5", "10.0.1.6", "10.0.1.7"]);
}
