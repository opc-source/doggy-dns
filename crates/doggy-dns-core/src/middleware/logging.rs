use async_trait::async_trait;
use doggy_dns_plugin::{Middleware, MiddlewareAction};
use hickory_server::server::Request;

pub struct LoggingMiddleware;

#[async_trait]
impl Middleware for LoggingMiddleware {
    async fn before_request(&self, request: &Request) -> MiddlewareAction {
        let request_id = request.metadata.id;
        if let Ok(info) = request.request_info() {
            tracing::info!(
                request_id = request_id,
                src = %info.src,
                query_name = %info.query.name(),
                query_type = ?info.query.query_type(),
                protocol = ?info.protocol,
                "dns query received"
            );
        }
        MiddlewareAction::Continue
    }

    async fn after_request(&self, request: &Request, duration_ms: u64) {
        let request_id = request.metadata.id;
        if let Ok(info) = request.request_info() {
            tracing::info!(
                request_id = request_id,
                query_name = %info.query.name(),
                duration_ms = duration_ms,
                "dns query completed"
            );
        }
    }
}
