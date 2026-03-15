#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::mapping::{parse_dns_name, to_dns_records};
use hickory_proto::rr::{Name, RecordType};
use std::str::FromStr;

#[test]
fn parse_valid_dns_name() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_some());
    let parts = result.unwrap();
    assert_eq!(parts.service_name, "user-service");
    assert_eq!(parts.group, "DEFAULT_GROUP");
    assert_eq!(parts.namespace, "public");
}

#[test]
fn parse_invalid_dns_name_wrong_zone() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.other.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_none());
}

#[test]
fn parse_invalid_dns_name_too_few_labels() {
    let name = Name::from_str("user-service.nacos.local.").unwrap();
    let result = parse_dns_name(&name, "nacos.local");
    assert!(result.is_none());
}

#[test]
fn to_dns_records_creates_a_records() {
    let ips = vec!["10.0.1.5".to_string(), "10.0.1.6".to_string()];
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl(), 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let ips = vec!["::1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_skips_invalid_ips() {
    let ips = vec!["not-an-ip".to_string(), "10.0.1.1".to_string()];
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &ips, 6);

    assert_eq!(records.len(), 1);
}
