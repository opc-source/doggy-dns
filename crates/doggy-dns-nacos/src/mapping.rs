use crate::watcher::{CachedInstance, KEY_SEPARATOR, ServiceKey};
use hickory_proto::rr::rdata::{A, AAAA};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType};
use std::borrow::Cow;
use std::net::IpAddr;

/// Build the cache key for a `<service>.<group>.<namespace>.<zone>` name.
///
/// `name` is a [`LowerName`], which is what the server hands every zone handler,
/// so the labels are already lowercase and no `to_lowercase` pass runs per
/// query - the type carries the guarantee the old code got from three
/// `to_lowercase()` calls on values that were already lower.
///
/// The zone suffix is dropped from the **right**, which is what keeps dotted
/// service names (`com.example.api`) working. Membership is *not* checked here:
/// callers must run [`Name::zone_of`] first, or a sibling like
/// `evilnacos.local` is read as if it belonged to `nacos.local`.
///
/// `None` means the name has fewer than three labels above the zone or carries a
/// label that is not UTF-8, contains the key separator, or contains a dot (the
/// escaped spelling of a dotted service). All are names the old string parser
/// could not have matched against a Nacos service either, so the authority's
/// authoritative `NXDOMAIN` is the same answer.
pub fn service_key_from_name(name: &LowerName, zone: &Name) -> Option<ServiceKey> {
    let zone_len = zone.iter().count();
    let name_len = name.iter().count();
    if name_len < zone_len + 3 {
        return None;
    }
    let service_len = name_len - zone_len - 2;

    let mut labels = name.iter();
    // Borrowed when the service is one label, which is the common case; a
    // dotted service pays one owned join for itself and one for the key.
    let mut service: Cow<'_, str> = Cow::Borrowed("");
    for (index, label) in labels.by_ref().take(service_len).enumerate() {
        let text = label_text(label)?;
        if index == 0 {
            service = Cow::Borrowed(text);
        } else {
            service.to_mut().push('.');
            service.to_mut().push_str(text);
        }
    }
    let group = labels.next().and_then(label_text)?;
    let namespace = labels.next().and_then(label_text)?;

    Some(ServiceKey::new(&service, group, namespace))
}

/// A DNS label as text.
///
/// Rejects the composite-key separator byte so a label can never forge a field
/// boundary, rejects a dot so the escaped single-label spelling of a dotted
/// service cannot alias the canonical multi-label one, and rejects non-UTF-8
/// (RFC 1035 binary) labels.
fn label_text(label: &[u8]) -> Option<&str> {
    if label.contains(&(KEY_SEPARATOR as u8)) || label.contains(&b'.') {
        return None;
    }
    std::str::from_utf8(label).ok()
}

/// Convert a service's instances to DNS records matching `rtype`.
///
/// Only records of the requested family are produced, so an `A` query never
/// returns `AAAA` records (and vice versa). The addresses are already parsed -
/// `CachedInstance` holds an `IpAddr` - so this no longer runs
/// `IpAddr::from_str` over every instance for every query.
pub fn to_dns_records(
    name: &Name,
    instances: &[CachedInstance],
    ttl: u32,
    rtype: RecordType,
) -> Vec<Record> {
    instances
        .iter()
        .filter_map(|instance| match (instance.ip, rtype) {
            (IpAddr::V4(ipv4), RecordType::A) => {
                Some(Record::from_rdata(name.clone(), ttl, RData::A(A(ipv4))))
            }
            (IpAddr::V6(ipv6), RecordType::AAAA) => Some(Record::from_rdata(
                name.clone(),
                ttl,
                RData::AAAA(AAAA(ipv6)),
            )),
            _ => None,
        })
        .collect()
}
