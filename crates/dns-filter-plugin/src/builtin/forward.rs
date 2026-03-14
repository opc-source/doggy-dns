use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::Resolver;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::net::SocketAddr;
use std::time::Duration;

pub struct ForwardAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
}

impl ForwardAuthority {
    pub async fn new(
        upstream: Vec<String>,
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        let name_servers: Vec<NameServerConfig> = upstream
            .iter()
            .map(|addr| {
                let socket_addr: SocketAddr = addr
                    .parse()
                    .unwrap_or_else(|_| format!("{}:53", addr).parse().unwrap());
                NameServerConfig::udp_and_tcp(socket_addr.ip())
            })
            .collect();

        let config = ResolverConfig::from_parts(None, vec![], name_servers);

        let mut opts = ResolverOpts::default();
        opts.cache_size = cache_size;
        opts.positive_min_ttl = Some(min_ttl);
        opts.positive_max_ttl = Some(max_ttl);

        let resolver =
            Resolver::builder_with_config(config, TokioRuntimeProvider::default())
                .with_options(opts)
                .build()?;

        let origin = LowerName::from(Name::root());
        Ok(Self { resolver, origin })
    }
}

#[async_trait]
impl ZoneHandler for ForwardAuthority {
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
