use crate::mapping::{parse_dns_name, to_dns_records};
use crate::watcher::{CachedInstance, ServiceKey};
use async_trait::async_trait;
use dashmap::DashMap;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, LookupRecords, ZoneHandler,
    ZoneType,
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
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Skip;
        }

        let query_name = Name::from(name.clone());
        let parts = match parse_dns_name(&query_name, &self.dns_zone) {
            Some(p) => p,
            None => return LookupControlFlow::Skip,
        };

        // Normalize to lowercase for case-insensitive DNS matching
        let key = ServiceKey {
            service_name: parts.service_name.to_lowercase(),
            group: parts.group.to_lowercase(),
            namespace: parts.namespace.to_lowercase(),
        };

        let instances = match self.cache.get(&key) {
            Some(entry) => entry.value().clone(),
            None => return LookupControlFlow::Skip,
        };

        let ips: Vec<String> = instances.iter().map(|i| i.ip.clone()).collect();
        let records = to_dns_records(&query_name, &ips, self.ttl);

        if records.is_empty() {
            return LookupControlFlow::Skip;
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
