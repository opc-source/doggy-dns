//! Test and benchmark conveniences.
//!
//! These cannot be gated behind `#[cfg(test)]`: integration tests in this crate
//! and its siblings (`doggy-dns-core`, `doggy-dns-plugin`) compile the library
//! without `cfg(test)`, and a private module's trait impls are still visible
//! crate-wide, so nothing here becomes part of the documented API.

use crate::authority::ServiceCatalog;
use crate::watcher::CachedInstance;
use dashmap::DashMap;

/// A plain map is a catalog as well: the authority tests and benchmarks fill one
/// directly instead of standing up a Nacos client. Production always hands in
/// the watcher. The borrowed `&str` a query yields looks the name up without
/// building anything.
impl ServiceCatalog for DashMap<String, Vec<CachedInstance>> {
    async fn instances(&self, service: &str) -> Option<Vec<CachedInstance>> {
        self.get(service).map(|entry| entry.value().clone())
    }
}
