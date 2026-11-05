use crate::mapping::{service_key_from_name, to_dns_records};
use crate::watcher::{CachedInstance, ServiceKey};
use async_trait::async_trait;
use dashmap::DashMap;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::SOA;
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType, TSigResponseContext};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupError, LookupOptions, LookupRecords,
    ZoneHandler, ZoneType,
};
use std::sync::Arc;

/// Where the authority reads a service's instances from on every query.
///
/// The authority keeps no copy of its own: whatever it answers is what the
/// catalog holds at that instant, so a deletion the catalog has absorbed can
/// never be served from a stale second copy.
///
/// `None` means the catalog does not know the service (authoritative NXDOMAIN).
/// `Some` means it does, and an empty list is a service with no usable instance
/// right now (NODATA), not an unknown name.
#[async_trait]
pub trait ServiceCatalog: Send + Sync {
    async fn instances(&self, key: &ServiceKey) -> Option<Vec<CachedInstance>>;
}

pub struct NacosAuthority {
    catalog: Arc<dyn ServiceCatalog>,
    ttl: u32,
    origin: LowerName,
    /// Apex SOA, built once. Every in-zone negative answer attaches it, and
    /// rebuilding it cost two `format!` plus two `Name::from_ascii` per query.
    apex_soa: Record,
}

impl NacosAuthority {
    /// Answer every query from `catalog`. Production hands in the Nacos client's
    /// cache; tests and benchmarks may hand in any map-like source.
    pub fn new(catalog: Arc<dyn ServiceCatalog>, dns_zone: &str, ttl: u32) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let zone = Name::from_ascii(&zone_name).unwrap_or_default();
        let origin = LowerName::from(zone.clone());
        let apex_soa = build_soa_record(&zone, ttl);
        Self {
            catalog,
            ttl,
            origin,
            apex_soa,
        }
    }

    /// Answer from the instances the catalog returned for this name.
    fn answer(
        &self,
        name: &LowerName,
        instances: &[CachedInstance],
        rtype: RecordType,
    ) -> LookupControlFlow<AuthLookup> {
        // The service exists, but we only serve address records.
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        // Answers carry an owned name: one clone for the answer itself (paid
        // even when the family filter then finds no record) plus one per
        // record. That is the floor a `Vec<Record>` gives us.
        let answer_name = Name::from(name.clone());
        let records = to_dns_records(&answer_name, instances, self.ttl, rtype);

        if records.is_empty() {
            // Service exists, but has no address of the requested family.
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::Section(records),
            None,
        )))
    }
}

/// A plain map is a catalog as well: the authority tests and benchmarks fill one
/// directly instead of standing up a Nacos client.
#[async_trait]
impl ServiceCatalog for DashMap<ServiceKey, Vec<CachedInstance>> {
    async fn instances(&self, key: &ServiceKey) -> Option<Vec<CachedInstance>> {
        self.get(key).map(|entry| entry.value().clone())
    }
}

/// The zone exists but this name does not, so the answer is authoritative and
/// must never fall through to a later plugin.
fn nxdomain() -> LookupControlFlow<AuthLookup> {
    LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
}

/// SOA for the zone apex.
///
/// The zone is synthesized from Nacos, so there is no real primary or serial:
/// the apex itself is the MNAME, `hostmaster.<zone>` the RNAME and the
/// configured record TTL doubles as the negative-caching TTL.
fn build_soa_record(zone: &Name, ttl: u32) -> Record {
    let admin = Name::from_ascii(format!("hostmaster.{zone}")).unwrap_or_default();
    let soa = SOA::new(zone.clone(), admin, 1, 3600, 600, 86400, ttl);
    Record::from_rdata(zone.clone(), ttl, RData::SOA(soa))
}

#[async_trait]
impl ZoneHandler for NacosAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        // Zone membership on labels: `zone_of` compares label bytes from the
        // right and allocates nothing, where the old string test built two
        // lowercased Strings plus a `format!(".{zone}")` - and ran twice per
        // lookup, here and inside the name parser.
        if !self.origin.zone_of(name) {
            return LookupControlFlow::Skip;
        }

        // Inside the zone we are authoritative: never fall through to
        // native/forward, or internal names could leak to an upstream resolver.
        // Equal label count plus `zone_of` is name equality.
        if name.iter().count() == self.origin.iter().count() {
            return if rtype == RecordType::SOA {
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::Section(vec![self.apex_soa.clone()]),
                    None,
                )))
            } else {
                // The zone exists, so every other apex type is NODATA.
                LookupControlFlow::Continue(Err(LookupError::NameExists))
            };
        }

        let Some(key) = service_key_from_name(name, &self.origin) else {
            return nxdomain();
        };

        // Every in-zone query reads the catalog, address type or not: without a
        // local copy this is the only way to tell an existing service (NODATA for
        // a type it does not serve) from an unknown one (NXDOMAIN).
        match self.catalog.instances(&key).await {
            Some(instances) => self.answer(name, &instances, rtype),
            None => nxdomain(),
        }
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(_) => return (LookupControlFlow::Skip, None),
        };
        let result = self
            .lookup(
                request_info.query.name(),
                request_info.query.query_type(),
                Some(&request_info),
                lookup_options,
            )
            .await;
        (result, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
