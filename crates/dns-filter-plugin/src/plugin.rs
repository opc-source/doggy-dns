use async_trait::async_trait;
use hickory_server::server::Request;

/// Result of middleware processing.
pub enum MiddlewareAction {
    /// Continue to the next middleware / authority chain.
    Continue,
    /// Short-circuit: skip remaining middleware and authority chain.
    /// The middleware has already sent the DNS response.
    ShortCircuit,
}

/// Cross-cutting middleware plugin (logging, metrics, etc.).
/// Wraps request processing with before/after hooks.
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Called before the authority chain processes the request.
    /// Return `ShortCircuit` to skip the authority chain.
    async fn before_request(&self, request: &Request) -> MiddlewareAction;

    /// Called after the authority chain has processed the request.
    async fn after_request(&self, request: &Request, duration_ms: u64);
}
