use crate::authority::CatalogAnswer;
use nacos_sdk::api::naming::ServiceInstance;
use std::net::IpAddr;
use std::str::FromStr;

/// Simplified service instance for DNS resolution.
#[derive(Debug, Clone)]
pub struct CachedInstance {
    pub ip: IpAddr,
    pub port: i32,
    pub healthy: bool,
}

impl CachedInstance {
    /// Build a record source, rejecting an instance whose address is not an IP.
    ///
    /// Parsed once per query read instead of once per record, so an unparsable
    /// address is dropped before the family filter walks the list.
    pub fn from_service_instance(instance: &ServiceInstance) -> Option<Self> {
        match IpAddr::from_str(instance.ip()) {
            Ok(ip) => Some(Self {
                ip,
                port: instance.port(),
                healthy: instance.healthy(),
            }),
            Err(e) => {
                tracing::warn!("skipping invalid IP '{}': {}", instance.ip(), e);
                None
            }
        }
    }
}

/// The instances worth answering with: healthy, enabled, and carrying an address
/// that parses. The client keeps instances this resolver cannot use; they are
/// dropped when a query reads the list.
pub fn usable_instances(instances: &[ServiceInstance]) -> Vec<CachedInstance> {
    instances
        .iter()
        .filter(|instance| instance.healthy() && instance.enabled())
        .filter_map(CachedInstance::from_service_instance)
        .collect()
}

/// What the client's cache can answer for `service`, if anything.
///
/// Some only when the list holds an instance worth serving. An empty list is
/// deliberately not an answer: `get_all_instances(subscribe = true)` reports a
/// fetch that failed exactly as it reports a service with no instances
/// (`naming/mod.rs:368-372`), so from this call shape empty proves nothing, and
/// answering NXDOMAIN on it would negative-cache an outage. What a `None` means is
/// the caller's decision - ask the server, or refuse.
pub fn cache_hit_of(service: &str, instances: &[ServiceInstance]) -> Option<CatalogAnswer> {
    let usable = usable_instances(instances);
    if usable.is_empty() {
        tracing::debug!(
            "the nacos client cache has nothing to answer '{}' with",
            service
        );
        return None;
    }
    Some(CatalogAnswer::Instances(usable))
}
