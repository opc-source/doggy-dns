use crate::mapping::{is_in_zone, parse_dns_name, to_dns_records};
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
    dns_zone: String,
    ttl: u32,
    origin: LowerName,
}

impl NacosAuthority {
    pub fn new(
        cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
        dns_zone: &str,
        ttl: u32,
    ) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let origin = LowerName::from(Name::from_ascii(&zone_name).unwrap_or_default());
        Self {
            cache,
            dns_zone: dns_zone.to_string(),
            ttl,
            origin,
        }
    }
    /// SOA for the zone apex.
    ///
    /// The zone is synthesized from Nacos, so there is no real primary or
    /// serial: the apex itself is the MNAME, `hostmaster.<zone>` the RNAME and
    /// the configured record TTL doubles as the negative-caching TTL.
    fn soa_record(&self) -> Record {
        let zone = Name::from_ascii(format!("{}.", self.dns_zone.trim_end_matches('.')))
            .unwrap_or_default();
        let admin = Name::from_ascii(format!("hostmaster.{zone}")).unwrap_or_default();
        let soa = SOA::new(zone.clone(), admin, 1, 3600, 600, 86400, self.ttl);
        Record::from_rdata(zone, self.ttl, RData::SOA(soa))
    }
}

fn is_zone_apex(name: &Name, zone_suffix: &str) -> bool {
    name.to_ascii()
        .trim_end_matches('.')
        .eq_ignore_ascii_case(zone_suffix.trim_end_matches('.'))
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
        let query_name = Name::from(name.clone());

        // Names outside our zone belong to another authority.
        if !is_in_zone(&query_name, &self.dns_zone) {
            return LookupControlFlow::Skip;
        }

        // Inside the zone we are authoritative: never fall through to
        // native/forward, or internal names could leak to an upstream resolver.
        if is_zone_apex(&query_name, &self.dns_zone) {
            return if rtype == RecordType::SOA {
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::Section(vec![self.soa_record()]),
                    None,
                )))
            } else {
                // The zone exists, so every other apex type is NODATA.
                LookupControlFlow::Continue(Err(LookupError::NameExists))
            };
        }

        let parts = match parse_dns_name(&query_name, &self.dns_zone) {
            Some(parts) => parts,
            None => {
                return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                    ResponseCode::NXDomain,
                )));
            }
        };

        // Normalize to lowercase for case-insensitive DNS matching
        let key = ServiceKey {
            service_name: parts.service_name.to_lowercase(),
            group: parts.group.to_lowercase(),
            namespace: parts.namespace.to_lowercase(),
        };

        let instances = match self.cache.get(&key) {
            Some(entry) => entry.value().clone(),
            None => {
                return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                    ResponseCode::NXDomain,
                )));
            }
        };

        // The service exists, but we only serve address records.
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        let ips: Vec<String> = instances.iter().map(|i| i.ip.clone()).collect();
        let records = to_dns_records(&query_name, &ips, self.ttl, rtype);

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
