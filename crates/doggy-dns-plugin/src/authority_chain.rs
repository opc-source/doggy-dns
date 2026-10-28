use hickory_proto::rr::{LowerName, Name, Record, RecordType};
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler, ZoneType,
};
use std::sync::Arc;

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
            .map(|(name, handler)| NamedHandler { name, handler })
            .collect();
        Self { handlers }
    }

    /// Resolve a query by iterating the chain.
    /// Returns the first non-Skip result with the handler name, or Skip if all handlers skip.
    pub async fn resolve(
        &self,
        name: &Name,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> ResolveResult {
        let lower_name = LowerName::new(name);
        let lookup_options = LookupOptions::default();

        for entry in &self.handlers {
            let result = entry
                .handler
                .lookup(&lower_name, record_type, request_info, lookup_options)
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
                        apex_soa(entry.handler.as_ref(), request_info).await
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
