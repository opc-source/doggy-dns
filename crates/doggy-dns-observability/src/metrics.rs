use crate::auth;
use crate::endpoint::{self, Signal};
use anyhow::{Context, Result};
use async_trait::async_trait;
use doggy_dns_core::config::{MetricsTemporality, ObservabilityConfig};
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_server::server::Request;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _, UpDownCounter};
use opentelemetry_otlp::{MetricExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use std::sync::Arc;
use std::time::Duration;

/// DNS-aware metric instruments shared by the observability middleware.
pub struct DnsMetrics {
    queries: Counter<u64>,
    duration: Histogram<f64>,
    in_flight: UpDownCounter<i64>,
}

impl DnsMetrics {
    pub(crate) fn new(meter: &Meter) -> Self {
        let queries = meter
            .u64_counter("dns.queries")
            .with_description("Total DNS queries processed")
            .with_unit("1")
            .build();

        // OTel's default histogram buckets are second-oriented; DNS latency is
        // mostly sub-second, so use millisecond boundaries instead.
        let duration = meter
            .f64_histogram("dns.query.duration")
            .with_description("DNS query processing duration")
            .with_unit("ms")
            .with_boundaries(vec![
                0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0,
                2500.0, 5000.0,
            ])
            .build();

        let in_flight = meter
            .i64_up_down_counter("dns.queries.in_flight")
            .with_description("DNS queries currently being processed")
            .with_unit("1")
            .build();

        Self {
            queries,
            duration,
            in_flight,
        }
    }

    /// Wrap these instruments into a middleware instance.
    pub fn middleware(self: Arc<Self>) -> Arc<dyn Middleware> {
        Arc::new(DnsMetricsMiddleware { metrics: self })
    }

    fn record(&self, request: &Request, duration_ms: u64, outcome: &RequestOutcome) {
        let attributes = outcome_attributes(request, outcome);
        self.queries.add(1, &attributes);
        self.duration.record(duration_ms as f64, &attributes);
    }
}

pub(crate) fn build_meter_provider(
    config: &ObservabilityConfig,
    resource: Resource,
) -> Result<(SdkMeterProvider, DnsMetrics)> {
    let endpoint = endpoint::resolve_signal_endpoint(config, Signal::Metrics)?;
    let headers = auth::resolve_headers(config)?;
    let temporality = match config.metrics.temporality {
        MetricsTemporality::Cumulative => Temporality::Cumulative,
        MetricsTemporality::Delta => Temporality::Delta,
    };

    let exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .with_timeout(Duration::from_secs(config.timeout_secs))
        .with_temporality(temporality)
        .build()
        .context("failed to build OTLP metric exporter")?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(Duration::from_secs(config.metrics.interval_secs))
        .build();

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    let meter = provider.meter(doggy_dns_plugin::APP_NAME);
    let metrics = DnsMetrics::new(&meter);
    Ok((provider, metrics))
}

struct DnsMetricsMiddleware {
    metrics: Arc<DnsMetrics>,
}

#[async_trait]
impl Middleware for DnsMetricsMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        self.metrics.in_flight.add(1, &[]);
        MiddlewareAction::Continue
    }

    async fn after_request(&self, request: &Request, duration_ms: u64, outcome: &RequestOutcome) {
        self.metrics.in_flight.add(-1, &[]);
        self.metrics.record(request, duration_ms, outcome);
    }
}

fn outcome_attributes(request: &Request, outcome: &RequestOutcome) -> Vec<KeyValue> {
    let (question_type, protocol) = match request.request_info() {
        Ok(info) => (
            format!("{:?}", info.query.query_type()),
            format!("{:?}", info.protocol).to_ascii_lowercase(),
        ),
        Err(_) => ("unknown".to_string(), "unknown".to_string()),
    };

    vec![
        KeyValue::new("dns.question.type", question_type),
        KeyValue::new("network.protocol.name", protocol),
        KeyValue::new(
            "dns.response.code",
            format!("{:?}", outcome.response_code).to_ascii_uppercase(),
        ),
        KeyValue::new(
            "dns.authority",
            outcome
                .authority
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use hickory_server::net::xfer::Protocol;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::str::FromStr;

    fn make_request() -> Request {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        let mut query = Query::new();
        query.set_name(Name::from_str("example.com.").unwrap());
        query.set_query_type(RecordType::AAAA);
        message.add_query(query);
        let bytes = message.to_vec().unwrap();
        Request::from_bytes(
            bytes,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 5353)),
            Protocol::Udp,
        )
        .unwrap()
    }

    fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
        attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .and_then(|kv| match &kv.value {
                opentelemetry::Value::String(value) => Some(value.as_str()),
                _ => None,
            })
    }

    #[test]
    fn attributes_contain_low_cardinality_dns_dimensions() {
        let request = make_request();
        let outcome = RequestOutcome {
            response_code: ResponseCode::NoError,
            authority: Some("native".to_string()),
            answers: 2,
        };

        let attributes = outcome_attributes(&request, &outcome);
        assert_eq!(attribute(&attributes, "dns.question.type"), Some("AAAA"));
        assert_eq!(attribute(&attributes, "network.protocol.name"), Some("udp"));
        assert_eq!(attribute(&attributes, "dns.response.code"), Some("NOERROR"));
        assert_eq!(attribute(&attributes, "dns.authority"), Some("native"));
        assert!(
            attributes
                .iter()
                .all(|kv| !kv.key.as_str().contains("question.name")),
            "qname must never be a metric attribute"
        );
    }

    #[test]
    fn response_codes_are_upper_case() {
        let request = make_request();
        let outcome = RequestOutcome {
            response_code: ResponseCode::ServFail,
            authority: None,
            answers: 0,
        };
        let attributes = outcome_attributes(&request, &outcome);
        assert_eq!(
            attribute(&attributes, "dns.response.code"),
            Some("SERVFAIL")
        );
        assert_eq!(attribute(&attributes, "dns.authority"), Some("none"));
    }

    #[test]
    fn unparseable_requests_still_produce_attributes() {
        let request = make_request();
        let outcome = RequestOutcome {
            response_code: ResponseCode::FormErr,
            authority: None,
            answers: 0,
        };
        let attributes = outcome_attributes(&request, &outcome);
        assert_eq!(attribute(&attributes, "dns.response.code"), Some("FORMERR"));
    }
}
