use async_trait::async_trait;
use hickory_proto::op::ResponseCode;
use hickory_server::server::Request;

/// Result of middleware processing.
pub enum MiddlewareAction {
    /// Continue to the next middleware / authority chain.
    Continue,
    /// Short-circuit: skip remaining middleware and authority chain.
    /// The middleware has already sent the DNS response.
    ShortCircuit,
}

/// Final outcome of a request, available to `after_request`.
///
/// This carries the response code, the authority that produced the answer and
/// the number of answer records. It is intentionally lightweight: metric
/// middleware must not inspect or serialize the DNS response itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOutcome {
    /// Response code sent to the client.
    pub response_code: ResponseCode,
    /// Authority that handled the query, e.g. `native` or `forward[8.8.8.8:53]`.
    pub authority: Option<String>,
    /// Number of answer records in the response.
    pub answers: usize,
}

/// Cross-cutting middleware plugin (logging, metrics, etc.).
/// Wraps request processing with before/after hooks.
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Called before the authority chain processes the request.
    /// Return `ShortCircuit` to skip the authority chain.
    async fn before_request(&self, request: &Request) -> MiddlewareAction;

    /// Called after the authority chain has processed the request.
    ///
    /// `outcome` carries the final response code, the authority that produced
    /// the answer, and the number of answer records.
    async fn after_request(&self, request: &Request, duration_ms: u64, outcome: &RequestOutcome);
}
