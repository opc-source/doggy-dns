use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler};
use std::sync::Arc;

/// Result of a chain resolution, including which handler responded.
pub struct ResolveResult {
    pub outcome: LookupControlFlow<AuthLookup>,
    /// Name of the handler that produced the result, or `None` if all skipped.
    pub handler_name: Option<String>,
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
                    return ResolveResult {
                        outcome: result,
                        handler_name: Some(entry.name.clone()),
                    };
                }
            }
        }

        ResolveResult {
            outcome: LookupControlFlow::Skip,
            handler_name: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}
