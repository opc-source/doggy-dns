use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::Resolver;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::time::Duration;

pub struct SystemAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
}

impl SystemAuthority {
    pub async fn new(
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        let mut builder = Resolver::builder_tokio()?;
        let opts = builder.options_mut();
        opts.cache_size = cache_size;
        opts.positive_min_ttl = Some(min_ttl);
        opts.positive_max_ttl = Some(max_ttl);
        let resolver = builder.build()?;

        let origin = LowerName::from(Name::root());
        Ok(Self { resolver, origin })
    }
}

#[async_trait]
impl ZoneHandler for SystemAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::External
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
        let name_str = name.to_string();
        match self.resolver.lookup(&name_str, rtype).await {
            Ok(lookup) => {
                if lookup.answers().is_empty() {
                    LookupControlFlow::Skip
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::Resolved(lookup)))
                }
            }
            Err(_) => LookupControlFlow::Skip,
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
