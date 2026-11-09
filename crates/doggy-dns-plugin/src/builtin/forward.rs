use crate::builtin::wire_query_name;
use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_resolver::Resolver;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_server::net::{DnsError, NetError};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupError, LookupOptions, ZoneHandler, ZoneType,
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
        upstream: Vec<SocketAddr>,
        cache_size: u64,
        min_ttl: Duration,
        max_ttl: Duration,
    ) -> Result<Self> {
        if upstream.is_empty() {
            anyhow::bail!("forward authority requires at least one upstream server");
        }

        let upstream_count = upstream.len();
        let name_servers: Vec<NameServerConfig> = upstream
            .iter()
            .map(|socket_addr| {
                let mut udp = ConnectionConfig::udp();
                udp.port = socket_addr.port();
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = socket_addr.port();
                NameServerConfig::new(socket_addr.ip(), true, vec![udp, tcp])
            })
            .collect();

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

/// Map a resolver failure to the DNS result it represents.
fn classify_error(error: &NetError) -> (ResponseCode, LookupError) {
    if error.is_nx_domain() {
        return (
            ResponseCode::NXDomain,
            LookupError::ResponseCode(ResponseCode::NXDomain),
        );
    }
    if error.is_no_records_found() {
        return (ResponseCode::NoError, LookupError::NameExists);
    }

    let code = match error {
        NetError::Dns(DnsError::ResponseCode(code)) => *code,
        _ => ResponseCode::ServFail,
    };
    (code, LookupError::ResponseCode(code))
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
        // See `wire_query_name`: a `&str` would make the resolver re-parse the
        // name under IDNA/STD3 rules and reject interior underscores such as
        // Nacos' `DEFAULT_GROUP`.
        let query_name = wire_query_name(name);
        let span = tracing::info_span!(
            "dns.forward",
            otel.kind = "client",
            "dns.question.name" = %name,
            "dns.question.type" = tracing::field::display(rtype),
            "dns.upstream.count" = self.upstream_count as u64,
            "dns.answers" = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        match self
            .resolver
            .lookup(query_name, rtype)
            .instrument(span.clone())
            .await
        {
            Ok(lookup) => {
                let answers = lookup.answers().len();
                span.record("dns.answers", tracing::field::display(answers));
                span.record("otel.status_code", "OK");
                if answers == 0 {
                    tracing::debug!(
                        query = %name,
                        qtype = ?rtype,
                        "upstream returned an empty answer"
                    );
                    LookupControlFlow::Continue(Err(LookupError::NameExists))
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::Resolved(lookup)))
                }
            }
            Err(e) => {
                let (response_code, lookup_error) = classify_error(&e);
                let negative = matches!(
                    response_code,
                    ResponseCode::NoError | ResponseCode::NXDomain
                );
                span.record("otel.status_code", if negative { "OK" } else { "ERROR" });
                if negative {
                    tracing::debug!(
                        query = %name,
                        qtype = ?rtype,
                        response_code = ?response_code,
                        detail = %e,
                        "upstream returned a negative answer"
                    );
                } else {
                    tracing::warn!(
                        query = %name,
                        qtype = ?rtype,
                        response_code = ?response_code,
                        error = %e,
                        "upstream forward lookup failed"
                    );
                }
                LookupControlFlow::Continue(Err(lookup_error))
            }
        }
    }

    async fn search(
        &self,
        request: &Request,
        lookup_options: LookupOptions,
    ) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>) {
        crate::zone_handler::search_via_lookup(self, request, lookup_options).await
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
