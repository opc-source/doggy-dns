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
    ///
    /// Shared rather than owned: the name belongs to the chain, and the handler
    /// copies it onward into every query's [`RequestOutcome`](crate::RequestOutcome),
    /// so a clone here is a refcount bump instead of an allocation per query.
    pub handler_name: Option<Arc<str>>,
    /// Whether the responding handler owns its zone (AA flag).
    pub authoritative: bool,
    /// Apex SOA to attach to negative answers for downstream caching.
    pub soa: Option<Arc<Record>>,
}

/// Named handler entry in the chain.
struct NamedHandler {
    name: Arc<str>,
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
    /// Shared, because every negative answer attaches it: cloning the record per
    /// query would re-copy the name and RDATA for a value that never changes. The
    /// shared value is read-only - the emit path only ever borrows a record - so
    /// anything that needs to change one must clone it first.
    ///
    /// The first negative answer's `RequestInfo` is the one the fetch sees,
    /// and every later query reuses that record. That is sound only while
    /// `<handler>.lookup(origin, SOA, ..)` does not vary by request: every
    /// authoritative handler today ignores the argument (`NacosAuthority`
    /// discards it), and the `ZoneType::External` handlers never reach this
    /// code. A future view/ACL-per-client handler would have to key this cache
    /// by client - note that in the comment if you add one.
    apex_soa: OnceLock<Option<Arc<Record>>>,
}

impl NamedHandler {
    /// Whether this handler owns its zone, which sets the `AA` flag and makes
    /// a negative answer carry the apex SOA.
    fn is_authoritative(&self) -> bool {
        matches!(
            self.handler.zone_type(),
            ZoneType::Primary | ZoneType::Secondary
        )
    }
}

/// Iterates through a chain of ZoneHandlers.
///
/// Stops at the first `Break`. A `Continue` is provisional: the first one is
/// remembered while later handlers are tried, and returned only if none of
/// them breaks. That lets a failed `native` lookup fall back to `forward`.
pub struct AuthorityChain {
    handlers: Vec<NamedHandler>,
}

impl AuthorityChain {
    pub fn new(handlers: Vec<(String, Arc<dyn ZoneHandler>)>) -> Self {
        let handlers = handlers
            .into_iter()
            .map(|(name, handler)| NamedHandler {
                name: Arc::from(name),
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
    /// Returns the first `Break` with the handler name. If no handler breaks,
    /// returns the first `Continue` as a fallback; if every handler skips,
    /// returns `Skip`.
    pub async fn resolve(
        &self,
        name: &LowerName,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> ResolveResult {
        let lookup_options = LookupOptions::default();
        // The first provisional result, kept only in case no handler breaks.
        let mut deferred: Option<(&NamedHandler, LookupControlFlow<AuthLookup>)> = None;

        for entry in &self.handlers {
            let result = entry
                .handler
                .lookup(name, record_type, request_info, lookup_options)
                .await;

            match result {
                LookupControlFlow::Skip => continue,
                result @ LookupControlFlow::Break(_) => {
                    let authoritative = entry.is_authoritative();
                    let soa = negative_soa(entry, authoritative, &result, request_info).await;
                    return ResolveResult {
                        outcome: result,
                        handler_name: Some(Arc::clone(&entry.name)),
                        authoritative,
                        soa,
                    };
                }
                // A provisional answer is a fallback, not a verdict: keep the
                // first one and let a later handler break with a final answer.
                result @ LookupControlFlow::Continue(_) => {
                    if deferred.is_none() {
                        deferred = Some((entry, result));
                    }
                }
            }
        }

        let Some((entry, outcome)) = deferred else {
            return ResolveResult {
                outcome: LookupControlFlow::Skip,
                handler_name: None,
                authoritative: false,
                soa: None,
            };
        };
        // Materialize the SOA only for the result that is actually returned, so
        // a fallback overwritten by a later `Break` costs no extra lookup.
        let authoritative = entry.is_authoritative();
        let soa = negative_soa(entry, authoritative, &outcome, request_info).await;
        ResolveResult {
            outcome,
            handler_name: Some(Arc::clone(&entry.name)),
            authoritative,
            soa,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// The apex SOA to attach to an authoritative negative answer.
async fn negative_soa(
    entry: &NamedHandler,
    authoritative: bool,
    result: &LookupControlFlow<AuthLookup>,
    request_info: Option<&RequestInfo<'_>>,
) -> Option<Arc<Record>> {
    if !authoritative
        || !matches!(
            result,
            LookupControlFlow::Continue(Err(_)) | LookupControlFlow::Break(Err(_))
        )
    {
        return None;
    }

    match entry.apex_soa.get() {
        Some(cached) => cached.clone(),
        None => {
            let fetched = apex_soa(entry.handler.as_ref(), request_info).await;
            // Only a hit is cached. A `None` can be a transient failure of the
            // handler's own SOA lookup, and pinning it would drop the SOA from
            // every later negative for the rest of the process; a miss retries,
            // as the uncached code did.
            fetched.map(|record| {
                let record = Arc::new(record);
                let _ = entry.apex_soa.set(Some(Arc::clone(&record)));
                record
            })
        }
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
