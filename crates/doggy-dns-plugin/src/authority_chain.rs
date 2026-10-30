use hickory_proto::rr::{LowerName, Record, RecordType};
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::sync::{Arc, OnceLock};

/// Result of a chain resolution, including which handler responded.
pub struct ResolveResult {
    pub outcome: LookupControlFlow<AuthLookup>,
    /// Name of the handler that produced the result, or `None` if all skipped.
    pub handler_name: Option<String>,
    /// Whether the responding handler owns its zone (AA flag).
    pub authoritative: bool,
    /// Apex SOA to attach to negative answers for downstream caching.
    pub soa: Option<Record>,
}

/// Named handler entry in the chain.
struct NamedHandler {
    name: String,
    handler: Arc<dyn ZoneHandler>,
    /// Apex SOA for this handler's negative answers, fetched once.
    ///
    /// Lazily filled rather than fetched in `new`, because `new` is synchronous
    /// and has 38 call sites. Two concurrent first-timers may both fetch; the
    /// loser's `set` is ignored, which is benign and keeps this off the
    /// async-lock path. The value goes stale only if plugin TTLs ever become
    /// hot-reloadable - the chain is rebuilt with the config in that case, as
    /// it already is for `[[plugins]]` changes.
    ///
    /// The first negative answer's `RequestInfo` is the one the fetch sees,
    /// and every later query reuses that record. That is sound only while
    /// `<handler>.lookup(origin, SOA, ..)` does not vary by request: every
    /// authoritative handler today ignores the argument (`NacosAuthority`
    /// discards it), and the `ZoneType::External` handlers never reach this
    /// code. A future view/ACL-per-client handler would have to key this cache
    /// by client - note that in the comment if you add one.
    apex_soa: OnceLock<Option<Record>>,
}

/// Iterates through a chain of ZoneHandlers.
/// Stops at the first handler that returns Continue or Break (not Skip).
pub struct AuthorityChain {
    handlers: Vec<NamedHandler>,
}

impl AuthorityChain {
    pub fn new(handlers: Vec<(String, Arc<dyn ZoneHandler>)>) -> Self {
        let handlers = handlers
            .into_iter()
            .map(|(name, handler)| NamedHandler {
                name,
                handler,
                apex_soa: OnceLock::new(),
            })
            .collect();
        Self { handlers }
    }

    /// Resolve a query by iterating the chain.
    ///
    /// Takes the already-lowercased name the server parsed: `ZoneHandler::lookup`
    /// wants a `&LowerName` anyway, so cloning and re-lowercasing here cost four
    /// allocations per request for nothing.
    /// Returns the first non-Skip result with the handler name, or Skip if all handlers skip.
    pub async fn resolve(
        &self,
        name: &LowerName,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> ResolveResult {
        let lookup_options = LookupOptions::default();

        for entry in &self.handlers {
            let result = entry
                .handler
                .lookup(name, record_type, request_info, lookup_options)
                .await;

            match &result {
                LookupControlFlow::Skip => continue,
                LookupControlFlow::Continue(_) | LookupControlFlow::Break(_) => {
                    let authoritative = matches!(
                        entry.handler.zone_type(),
                        ZoneType::Primary | ZoneType::Secondary
                    );
                    let soa = if authoritative
                        && matches!(
                            result,
                            LookupControlFlow::Continue(Err(_)) | LookupControlFlow::Break(Err(_))
                        ) {
                        match entry.apex_soa.get() {
                            Some(cached) => cached.clone(),
                            None => {
                                let fetched = apex_soa(entry.handler.as_ref(), request_info).await;
                                // Only a hit is cached. A `None` can be a
                                // transient failure of the handler's own SOA
                                // lookup, and pinning it would drop the SOA
                                // from every later negative for the rest of
                                // the process; a miss retries, as the
                                // uncached code did.
                                if fetched.is_some() {
                                    let _ = entry.apex_soa.set(fetched.clone());
                                }
                                fetched
                            }
                        }
                    } else {
                        None
                    };

                    return ResolveResult {
                        outcome: result,
                        handler_name: Some(entry.name.clone()),
                        authoritative,
                        soa,
                    };
                }
            }
        }

        ResolveResult {
            outcome: LookupControlFlow::Skip,
            handler_name: None,
            authoritative: false,
            soa: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// Fetch the apex SOA of an authoritative handler for negative responses.
async fn apex_soa(
    handler: &dyn ZoneHandler,
    request_info: Option<&RequestInfo<'_>>,
) -> Option<Record> {
    let origin = handler.origin().clone();
    match handler
        .lookup(
            &origin,
            RecordType::SOA,
            request_info,
            LookupOptions::default(),
        )
        .await
    {
        LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
            lookup.iter().next().cloned()
        }
        _ => None,
    }
}
