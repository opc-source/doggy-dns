use async_trait::async_trait;
use dns_filter_plugin::{Middleware, MiddlewareAction};
use hickory_server::server::Request;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct MetricsMiddleware {
    queries_total: AtomicU64,
}

impl MetricsMiddleware {
    pub fn new() -> Self {
        Self {
            queries_total: AtomicU64::new(0),
        }
    }

    pub fn total_queries(&self) -> u64 {
        self.queries_total.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Middleware for MetricsMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        self.queries_total.fetch_add(1, Ordering::Relaxed);
        MiddlewareAction::Continue
    }

    async fn after_request(&self, _request: &Request, _duration_ms: u64) {
        // Phase 1: just count. Histogram deferred.
    }
}
