use hickory_proto::rr::TSigResponseContext;
use hickory_server::server::Request;
use hickory_server::zone_handler::{AuthLookup, LookupControlFlow, LookupOptions, ZoneHandler};

/// Implement [`ZoneHandler::search`] from a handler's own [`ZoneHandler::lookup`].
///
/// Every authority here serves a search the same way: it resolves the request's
/// name through itself and attaches no TSIG context. Delegating keeps that rule
/// in one place instead of three identical bodies.
pub async fn search_via_lookup<Z>(
    handler: &Z,
    request: &Request,
    lookup_options: LookupOptions,
) -> (LookupControlFlow<AuthLookup>, Option<TSigResponseContext>)
where
    Z: ZoneHandler + ?Sized,
{
    let request_info = match request.request_info() {
        Ok(info) => info,
        Err(_) => return (LookupControlFlow::Skip, None),
    };
    let result = handler
        .lookup(
            request_info.query.name(),
            request_info.query.query_type(),
            Some(&request_info),
            lookup_options,
        )
        .await;
    (result, None)
}
