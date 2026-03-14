use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::server::RequestInfo;
use hickory_server::zone_handler::{AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler};
use std::sync::Arc;

/// Iterates through a chain of ZoneHandlers.
/// Stops at the first handler that returns Continue or Break (not Skip).
pub struct AuthorityChain {
    handlers: Vec<Arc<dyn ZoneHandler>>,
}

impl AuthorityChain {
    pub fn new(handlers: Vec<Arc<dyn ZoneHandler>>) -> Self {
        Self { handlers }
    }

    /// Resolve a query by iterating the chain.
    /// Returns the first non-Skip result, or Skip if all handlers skip.
    pub async fn resolve(
        &self,
        name: &Name,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> LookupControlFlow<AuthLookup> {
        let lower_name = LowerName::new(name);
        let lookup_options = LookupOptions::default();

        for handler in &self.handlers {
            let result = handler
                .lookup(&lower_name, record_type, request_info, lookup_options)
                .await;

            match &result {
                LookupControlFlow::Skip => continue,
                LookupControlFlow::Continue(_) | LookupControlFlow::Break(_) => return result,
            }
        }

        LookupControlFlow::Skip
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}
