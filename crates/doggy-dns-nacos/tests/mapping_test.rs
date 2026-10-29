#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::mapping::{service_key_from_name, to_dns_records};
use doggy_dns_nacos::watcher::ServiceKey;
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

#[test]
fn to_dns_records_creates_a_records() {
    let ips = vec!["10.0.1.5".to_string(), "10.0.1.6".to_string()];
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6, RecordType::A);

    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl, 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let ips = vec!["::1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6, RecordType::AAAA);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_filters_a_records_for_a_query() {
    let ips = vec!["10.0.1.5".to_string(), "2001:db8::1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6, RecordType::A);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::A);
}

#[test]
fn to_dns_records_filters_aaaa_records_for_aaaa_query() {
    let ips = vec!["10.0.1.5".to_string(), "2001:db8::1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6, RecordType::AAAA);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_skips_invalid_ips() {
    let ips = vec!["not-an-ip".to_string(), "10.0.1.1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6, RecordType::A);

    assert_eq!(records.len(), 1);
}
