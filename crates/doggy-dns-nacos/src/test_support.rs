//! Test and benchmark conveniences.
//!
//! These cannot be gated behind `#[cfg(test)]`: this crate's integration tests
//! (`authority_test`, `mapping_test`) and `benches/nacos_bench.rs` compile the
//! library without `cfg(test)`. The module is `#[doc(hidden)] pub` so those
//! targets can import `qname`/`instances` through the library path; a private
//! module's trait impls were already visible crate-wide, so exposing the module
//! adds only those functions, and `doc(hidden)` keeps them out of the documented
//! API.

use crate::authority::{CatalogAnswer, ServiceCatalog};
use crate::watcher::CachedInstance;
use dashmap::DashMap;
use hickory_proto::rr::{LowerName, Name};
use std::str::FromStr;

/// A plain map is a catalog as well: the authority tests and benchmarks fill one
/// directly instead of standing up a Nacos client. Production always hands in
/// the watcher. The borrowed `&str` a query yields looks the name up without
/// building anything.
impl ServiceCatalog for DashMap<String, Vec<CachedInstance>> {
    async fn instances(&self, service: &str) -> CatalogAnswer {
        match self.get(service) {
            Some(entry) => CatalogAnswer::Instances(entry.value().clone()),
            None => CatalogAnswer::Unknown,
        }
    }
}

/// A query name as the authority receives it: case-folded by `LowerName`.
pub fn qname(text: &str) -> LowerName {
    LowerName::from(Name::from_str(text).expect("valid test name"))
}

/// `ips` as cache instances on the port every test uses.
pub fn instances(ips: &[&str]) -> Vec<CachedInstance> {
    ips.iter()
        .map(|ip| CachedInstance {
            ip: ip.parse().expect("test address"),
            port: 8080,
            healthy: true,
        })
        .collect()
}
