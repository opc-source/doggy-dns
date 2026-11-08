use crate::mapping::{service_name_from_name, to_dns_records};
use crate::watcher::CachedInstance;
use async_trait::async_trait;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::SOA;
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType, TSigResponseContext};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupError, LookupOptions, LookupRecords,
    ZoneHandler, ZoneType,
};
use std::future::Future;
use std::sync::Arc;

/// What a [`ServiceCatalog`] knows about one service.
#[derive(Debug, Clone)]
pub enum CatalogAnswer {
    /// The service exists. An empty list is a service with no usable instance
    /// right now, which the authority answers as NODATA.
    Instances(Vec<CachedInstance>),
    /// The catalog does not know the name (authoritative NXDOMAIN).
    Unknown,
    /// The catalog could not answer because its backend failed. This must not
    /// be reported as a negative answer: a client that caches NXDOMAIN would
    /// keep "this name does not exist" for the whole TTL, long after the
    /// outage ended.
    Unavailable,
}

/// Where the authority reads a service's instances from on every query.
///
/// The authority keeps no copy of its own: whatever it answers is what the
/// catalog holds at that instant, so a deletion the catalog has absorbed can
/// never be served from a stale second copy.
///
/// A service name, not a key: one catalog belongs to one Nacos group and
/// namespace, which [`NacosAuthority`] checks off the DNS name before it gets
/// here, so nothing downstream can tell two groups apart anyway.
///
/// Generic rather than `dyn`: this call sits on the query path, so the concrete
/// catalog lets it inline without a vtable hop or a boxed future.
pub trait ServiceCatalog: Send + Sync {
    fn instances(&self, service: &str) -> impl Future<Output = CatalogAnswer> + Send;
}

pub struct NacosAuthority<C: ServiceCatalog + 'static> {
    catalog: Arc<C>,
    ttl: u32,
    origin: LowerName,
    /// The group and namespace this zone serves, matched case-insensitively
    /// against the two labels above the zone.
    group: String,
    namespace: String,
    /// Apex SOA, built once. Every in-zone negative answer attaches it, and
    /// rebuilding it cost two `format!` plus two `Name::from_ascii` per query.
    apex_soa: Record,
}

impl<C: ServiceCatalog + 'static> NacosAuthority<C> {
    /// Answer every query from `catalog`. Production hands in the Nacos client's
    /// cache; tests and benchmarks may hand in any map-like source.
    pub fn new(catalog: Arc<C>, dns_zone: &str, group: &str, namespace: &str, ttl: u32) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let zone = Name::from_ascii(&zone_name).unwrap_or_default();
        let origin = LowerName::from(zone.clone());
        let apex_soa = build_soa_record(&zone, ttl);
        Self {
            catalog,
            ttl,
            origin,
            group: group.to_string(),
            namespace: namespace.to_string(),
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

        // The record owner is built per record inside the builder, from the
        // `LowerName` this already holds: no owned name is allocated here, so a
        // service with no address of the requested family allocates nothing to
        // answer NODATA.
        let records = to_dns_records(name, instances, self.ttl, rtype);

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

/// The zone exists but this name does not, so the answer is authoritative and
/// must never fall through to a later plugin.
fn nxdomain() -> LookupControlFlow<AuthLookup> {
    LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
}

/// The backend behind the catalog failed, so the name's existence is unknown.
/// SERVFAIL keeps the failure transient for the client instead of turning it
/// into a negative answer that outlives the outage.
fn servfail() -> LookupControlFlow<AuthLookup> {
    LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
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
impl<C: ServiceCatalog + 'static> ZoneHandler for NacosAuthority<C> {
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

        // The name has to sit below this plugin's group and namespace, which the
        // parser checks and reports by returning `None`: a catalog belongs to one
        // pair, so a name below another is not this zone's to answer.
        let Some(service) =
            service_name_from_name(name, &self.origin, &self.group, &self.namespace)
        else {
            return nxdomain();
        };

        // Every in-zone query reads the catalog, address type or not: without a
        // local copy this is the only way to tell an existing service (NODATA for
        // a type it does not serve) from an unknown one (NXDOMAIN).
        match self.catalog.instances(&service).await {
            CatalogAnswer::Instances(instances) => self.answer(name, &instances, rtype),
            CatalogAnswer::Unknown => nxdomain(),
            CatalogAnswer::Unavailable => servfail(),
        }
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        doggy_dns_plugin::zone_handler::search_via_lookup(self, request, lookup_options).await
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
