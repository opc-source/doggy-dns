//! Prometheus pull support for the control plane.
//!
//! The observability layer owns the OpenTelemetry meter provider and the
//! registry its Prometheus reader fills; the control plane only renders that
//! registry as text when `/metrics` is scraped.

use prometheus::{Encoder, Registry, TextEncoder};

/// Prometheus registry exposed at `GET /metrics`.
#[derive(Clone, Debug)]
pub struct MetricsRegistry {
    registry: Registry,
}

impl MetricsRegistry {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Gather every family and encode it in the Prometheus text exposition
    /// format. Called once per scrape, never on the DNS query path.
    pub fn render(&self) -> Result<Vec<u8>, prometheus::Error> {
        let mut body = Vec::new();
        TextEncoder::new().encode(&self.registry.gather(), &mut body)?;
        Ok(body)
    }
}
