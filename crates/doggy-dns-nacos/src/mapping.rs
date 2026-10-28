use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsNameParts {
    pub service_name: String,
    pub group: String,
    pub namespace: String,
}

/// Return true when `name` is the zone apex or a name below it.
///
/// The comparison is label-aligned: `evilnacos.local` is *not* inside the
/// `nacos.local` zone even though it shares the string suffix.
pub fn is_in_zone(name: &Name, zone_suffix: &str) -> bool {
    let name_str = name.to_ascii().trim_end_matches('.').to_ascii_lowercase();
    let zone = zone_suffix.trim_end_matches('.').to_ascii_lowercase();

    name_str == zone || name_str.ends_with(&format!(".{zone}"))
}

/// Parse a DNS query name into service parts.
/// Expected format: `<service>.<group>.<namespace>.<zone_suffix>.`
///
/// The name is split from the right, so service names may contain dots
/// (`com.example.api.DEFAULT_GROUP.public.nacos.local.`). Returns `None` when
/// the name is not below the zone or does not carry at least three labels.
pub fn parse_dns_name(name: &Name, zone_suffix: &str) -> Option<DnsNameParts> {
    if !is_in_zone(name, zone_suffix) {
        return None;
    }

    let name_str = name.to_ascii();
    let name_str = name_str.trim_end_matches('.');

    let suffix = zone_suffix.trim_end_matches('.');
    let prefix = name_str[..name_str.len() - suffix.len()].trim_end_matches('.');

    let parts: Vec<&str> = prefix.split('.').collect();
    if parts.len() < 3 {
        return None;
    }

    let (service, tail) = parts.split_at(parts.len() - 2);
    Some(DnsNameParts {
        service_name: service.join("."),
        group: tail[0].to_string(),
        namespace: tail[1].to_string(),
    })
}

/// Convert a list of IP strings to DNS records matching `rtype`.
///
/// Only records of the requested family are produced, so an `A` query never
/// returns `AAAA` records (and vice versa). Invalid IPs are skipped with a
/// warning log.
pub fn to_dns_records(name: &Name, ips: &[String], ttl: u32, rtype: RecordType) -> Vec<Record> {
    ips.iter()
        .filter_map(|ip_str| match (IpAddr::from_str(ip_str), rtype) {
            (Ok(IpAddr::V4(ipv4)), RecordType::A) => {
                Some(Record::from_rdata(name.clone(), ttl, RData::A(A(ipv4))))
            }
            (Ok(IpAddr::V6(ipv6)), RecordType::AAAA) => Some(Record::from_rdata(
                name.clone(),
                ttl,
                RData::AAAA(AAAA(ipv6)),
            )),
            (Ok(_), _) => None,
            (Err(e), _) => {
                tracing::warn!("skipping invalid IP '{}': {}", ip_str, e);
                None
            }
        })
        .collect()
}
