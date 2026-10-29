use anyhow::Result;
use async_trait::async_trait;
use hickory_proto::rr::{LowerName, Name, RecordType, TSigResponseContext};
use hickory_resolver::Resolver;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::time::Duration;

pub struct NativeAuthority {
    resolver: Resolver<TokioRuntimeProvider>,
    origin: LowerName,
}

impl NativeAuthority {
    pub async fn new(cache_size: u64, min_ttl: Duration, max_ttl: Duration) -> Result<Self> {
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
impl ZoneHandler for NativeAuthority {
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
        // See ForwardAuthority::lookup: passing a `&str` makes the resolver
        // re-parse the name under IDNA/STD3 rules, which reject interior
        // underscores such as Nacos' `DEFAULT_GROUP`. Use the wire-parsed Name.
        let query_name = Name::from(name.clone());
        match self.resolver.lookup(query_name, rtype).await {
            Ok(lookup) => {
                if lookup.answers().is_empty() {
                    tracing::debug!(
                        query = %name_str,
                        qtype = ?rtype,
                        "system resolver returned an empty answer, falling through"
                    );
                    LookupControlFlow::Skip
                } else {
                    LookupControlFlow::Continue(Ok(AuthLookup::Resolved(lookup)))
                }
            }
            Err(e) if e.is_nx_domain() || e.is_no_records_found() => {
                tracing::debug!(
                    query = %name_str,
                    qtype = ?rtype,
                    detail = %e,
                    "system resolver returned a negative answer, falling through"
                );
                LookupControlFlow::Skip
            }
            Err(e) => {
                tracing::warn!(
                    query = %name_str,
                    qtype = ?rtype,
                    error = %e,
                    "system resolver lookup failed, falling through"
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
