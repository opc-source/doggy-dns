use crate::builtin::wire_query_name;
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
        // See `wire_query_name`: a `&str` would make the resolver re-parse the
        // name under IDNA/STD3 rules and reject interior underscores such as
        // Nacos' `DEFAULT_GROUP`.
        let query_name = wire_query_name(name);
        match self.resolver.lookup(query_name, rtype).await {
            Ok(lookup) => {
                if lookup.answers().is_empty() {
                    tracing::debug!(
                        query = %name,
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
                    query = %name,
                    qtype = ?rtype,
                    detail = %e,
                    "system resolver returned a negative answer, falling through"
                );
                LookupControlFlow::Skip
            }
            Err(e) => {
                tracing::warn!(
                    query = %name,
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
