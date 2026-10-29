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

pub struct NacosAuthority {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    ttl: u32,
    origin: LowerName,
    /// Apex SOA, built once. Every in-zone negative answer attaches it, and
    /// rebuilding it cost two `format!` plus two `Name::from_ascii` per query.
    apex_soa: Record,
}

impl NacosAuthority {
    pub fn new(
        cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
        dns_zone: &str,
        ttl: u32,
    ) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let zone = Name::from_ascii(&zone_name).unwrap_or_default();
        let origin = LowerName::from(zone.clone());
        let apex_soa = build_soa_record(&zone, ttl);
        Self {
            cache,
            ttl,
            origin,
            apex_soa,
        }
    }
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

        let key = match service_key_from_name(name, &self.origin) {
            Some(key) => key,
            None => {
                return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                    ResponseCode::NXDomain,
                )));
            }
        };

        // Read through the guard instead of cloning the whole Vec: an answer
        // needs only borrowed instances now. Nothing below may await while
        // `entry` is alive, or a slow shard holder could stall a push writing
        // the same shard - clippy's await_holding_lock does not know about
        // DashMap, so the alloc guard and the async tests are the tripwire.
        let Some(entry) = self.cache.get(&key) else {
            return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                ResponseCode::NXDomain,
            )));
        };

        // The service exists, but we only serve address records.
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        // Answers carry an owned name, which only this path pays for. K records
        // cost K Name clones; that is the floor a Vec<Record> gives us.
        let answer_name = Name::from(name.clone());
        let records = to_dns_records(&answer_name, entry.value(), self.ttl, rtype);

        if records.is_empty() {
            // Service exists, but has no address of the requested family.
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::Section(records),
            None,
        )))
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
