use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_resolver::Resolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::net::SocketAddr;
use std::time::Duration;
use tracing::Instrument;

pub struct ForwardAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
    upstream_count: usize,
}

impl ForwardAuthority {
    pub async fn new(
        upstream: Vec<String>,
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        let upstream_count = upstream.len();
        let name_servers: Vec<NameServerConfig> = upstream
            .iter()
            .map(|addr| {
                let socket_addr: SocketAddr = addr
                    .parse()
                    .or_else(|_| format!("{}:53", addr).parse())
                    .map_err(|e| anyhow::anyhow!("invalid upstream address '{}': {}", addr, e))?;
                Ok(NameServerConfig::udp_and_tcp(socket_addr.ip()))
            })
            .collect::<Result<Vec<_>>>()?;

        let config = ResolverConfig::from_parts(None, vec![], name_servers);

        let mut opts = ResolverOpts::default();
        opts.cache_size = cache_size;
        opts.positive_min_ttl = Some(min_ttl);
        opts.positive_max_ttl = Some(max_ttl);

        let resolver = Resolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(opts)
            .build()?;

        let origin = LowerName::from(Name::root());
        Ok(Self {
            resolver,
            origin,
            upstream_count,
        })
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
        let span = tracing::info_span!(
            "dns.forward",
            otel.kind = "client",
            "dns.question.name" = %name_str,
            "dns.question.type" = tracing::field::display(rtype),
            "dns.upstream.count" = self.upstream_count as u64,
            "dns.answers" = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        match self
            .resolver
            .lookup(&name_str, rtype)
            .instrument(span.clone())
            .await
        {
            Ok(lookup) => {
                let answers = lookup.answers().len();
                span.record("dns.answers", tracing::field::display(answers));
                span.record("otel.status_code", "OK");
                if answers == 0 {
                    LookupControlFlow::Skip
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::Resolved(lookup)))
                }
            }
            Err(e) => {
                span.record("otel.status_code", "ERROR");
                tracing::debug!(
                    query = %name_str,
                    qtype = ?rtype,
                    error = %e,
                    "upstream forward lookup failed"
                );
                LookupControlFlow::Skip
            }
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
