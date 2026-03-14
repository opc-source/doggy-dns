use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{Name, RData, Record};
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsNameParts {
    pub service_name: String,
    pub group: String,
    pub namespace: String,
}

/// Parse a DNS query name into service parts.
/// Expected format: `<service>.<group>.<namespace>.<zone_suffix>.`
/// Returns None if the name doesn't match the expected zone.
pub fn parse_dns_name(name: &Name, zone_suffix: &str) -> Option<DnsNameParts> {
    let name_str = name.to_ascii();
    let name_str = name_str.trim_end_matches('.');

    let suffix = zone_suffix.trim_end_matches('.');
    if !name_str.ends_with(suffix) {
        return None;
    }

    let prefix = name_str[..name_str.len() - suffix.len()].trim_end_matches('.');

    let parts: Vec<&str> = prefix.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    Some(DnsNameParts {
        service_name: parts[0].to_string(),
        group: parts[1].to_string(),
        namespace: parts[2].to_string(),
    })
}

/// Convert a list of IP strings to DNS A/AAAA records.
/// Invalid IPs are skipped with a warning log.
pub fn to_dns_records(name: &Name, ips: &[String], ttl: u32) -> Vec<Record> {
    ips.iter()
        .filter_map(|ip_str| match IpAddr::from_str(ip_str) {
            Ok(IpAddr::V4(ipv4)) => Some(Record::from_rdata(name.clone(), ttl, RData::A(A(ipv4)))),
            Ok(IpAddr::V6(ipv6)) => Some(Record::from_rdata(
                name.clone(),
                ttl,
                RData::AAAA(AAAA(ipv6)),
            )),
            Err(e) => {
                tracing::warn!("skipping invalid IP '{}': {}", ip_str, e);
                None
            }
        })
        .collect()
}
