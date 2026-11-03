use async_trait::async_trait;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_server::server::Request;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Switch for query logging, shared between the middleware and whoever owns
/// the toggle.
///
/// The middleware chain is not rebuilt on a config reload (it holds the live
/// XDP cache map and the metrics instruments), so the `middleware.logging`
/// flip is an atomic store rather than a swap of the chain.
#[derive(Clone)]
pub struct LoggingToggle {
    enabled: Arc<AtomicBool>,
}

impl LoggingToggle {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn set(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

pub struct LoggingMiddleware {
    toggle: LoggingToggle,
}

impl LoggingMiddleware {
    pub fn with_toggle(toggle: LoggingToggle) -> Self {
        Self { toggle }
    }
}

#[async_trait]
impl Middleware for LoggingMiddleware {
    async fn before_request(&self, request: &Request) -> MiddlewareAction {
        if !self.toggle.is_enabled() {
            return MiddlewareAction::Continue;
        }
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

    async fn after_request(&self, request: &Request, duration_ms: u64, outcome: &RequestOutcome) {
        if !self.toggle.is_enabled() {
            return;
        }
        let request_id = request.metadata.id;
        if let Ok(info) = request.request_info() {
            tracing::info!(
                request_id = request_id,
                query_name = %info.query.name(),
                duration_ms = duration_ms,
                response_code = ?outcome.response_code,
                authority = outcome.authority.as_deref().unwrap_or("none"),
                answers = outcome.answers,
                "dns query completed"
            );
        }
    }
}
