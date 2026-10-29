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
use std::net::{IpAddr, SocketAddr};
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
        if upstream.is_empty() {
            anyhow::bail!("forward authority requires at least one upstream server");
        }

        let upstream_count = upstream.len();
        let name_servers: Vec<NameServerConfig> = upstream
            .iter()
            .map(|addr| {
                let socket_addr = parse_upstream(addr)?;
                let mut udp = ConnectionConfig::udp();
                udp.port = socket_addr.port();
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = socket_addr.port();
                Ok(NameServerConfig::new(
                    socket_addr.ip(),
                    true,
                    vec![udp, tcp],
                ))
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

/// Parse `IP` or `IP:port`, defaulting to port 53.
fn parse_upstream(addr: &str) -> Result<SocketAddr> {
    if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
        return Ok(socket_addr);
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }
    anyhow::bail!(
        "invalid upstream address '{}': expected IP or IP:port",
        addr
    )
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
        let name_str = name.to_string();
        // Hand the resolver the name as already parsed from the wire. Going
        // through `&str` would re-parse it under IDNA/STD3 rules, which reject
        // interior underscores (Nacos' `DEFAULT_GROUP`) and turn a name the
        // upstream could answer into a local SERVFAIL.
        let query_name = Name::from(name.clone());
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
                        query = %name_str,
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
                        query = %name_str,
                        qtype = ?rtype,
                        response_code = ?response_code,
                        detail = %e,
                        "upstream returned a negative answer"
                    );
                } else {
                    tracing::warn!(
                        query = %name_str,
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
