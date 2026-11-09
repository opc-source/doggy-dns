use crate::endpoint::{self, Signal};
use crate::exporter::configured_http;
use anyhow::{Context, Result};
use async_trait::async_trait;
use doggy_dns_basic::{MetricsTemporality, ObservabilityConfig};
use doggy_dns_plugin::naming::{protocol_name, record_type_name, response_code_name};
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_server::server::Request;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _, UpDownCounter};
use opentelemetry::{KeyValue, Value};
use opentelemetry_otlp::MetricExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use std::borrow::Cow;
use std::collections::HashMap;
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

/// Kernel-side fast-path counters produced by the XDP program.
///
/// One counter with a low-cardinality `dns.xdp.result` attribute; kernel-served
/// hits never reach [`DnsMetrics`], so keeping the instruments separate avoids a
/// sparse attribute set.
pub struct XdpMetrics {
    queries: Counter<u64>,
}

impl XdpMetrics {
    pub(crate) fn new(meter: &Meter) -> Self {
        let queries = meter
            .u64_counter("dns.xdp.queries")
            .with_description("DNS queries classified by the XDP fast path")
            .with_unit("1")
            .build();
        Self { queries }
    }

    /// Record a per-result delta. Zero counts are skipped so an idle interval
    /// does not create an empty time series.
    pub fn record(&self, result: &'static str, count: u64) {
        if count == 0 {
            return;
        }
        self.queries
            .add(count, &[KeyValue::new("dns.xdp.result", result)]);
    }
}

pub(crate) fn build_meter_provider(
    config: &ObservabilityConfig,
    resource: Resource,
    headers: HashMap<String, String>,
) -> Result<(SdkMeterProvider, DnsMetrics, XdpMetrics)> {
    let endpoint = endpoint::resolve_signal_endpoint(config, Signal::Metrics)?;
    let temporality = match config.metrics.temporality {
        MetricsTemporality::Cumulative => Temporality::Cumulative,
        MetricsTemporality::Delta => Temporality::Delta,
    };

    let exporter = configured_http(
        MetricExporter::builder().with_http(),
        endpoint,
        headers,
        config.timeout_secs,
    )
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
    let xdp_metrics = XdpMetrics::new(&meter);
    Ok((provider, metrics, xdp_metrics))
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

/// The four low-cardinality dimensions every query outcome is recorded under.
///
/// The common path hands OTel borrowed and ref-counted strings, so the attribute
/// set itself allocates nothing.
fn outcome_attributes(request: &Request, outcome: &RequestOutcome) -> [KeyValue; 4] {
    let (question_type, protocol) = match request.request_info() {
        Ok(info) => (
            record_type_name(info.query.query_type()),
            protocol_name(info.protocol),
        ),
        Err(_) => (Cow::Borrowed("unknown"), Cow::Borrowed("unknown")),
    };

    [
        KeyValue::new("dns.question.type", question_type),
        KeyValue::new("network.protocol.name", protocol),
        KeyValue::new(
            "dns.response.code",
            response_code_name(outcome.response_code),
        ),
        KeyValue::new(
            "dns.authority",
            outcome
                .authority
                .clone()
                .map_or(Value::from("none"), Value::from),
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
        make_request_with(RecordType::AAAA, Protocol::Udp)
    }

    fn make_request_with(query_type: RecordType, protocol: Protocol) -> Request {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        let mut query = Query::new();
        query.set_name(Name::from_str("example.com.").unwrap());
        query.set_query_type(query_type);
        message.add_query(query);
        let bytes = message.to_vec().unwrap();
        Request::from_bytes(
            bytes,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 5353)),
            protocol,
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
        let outcome = RequestOutcome::success("native".into(), 2, None, None);

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
        let outcome = RequestOutcome::error(ResponseCode::ServFail);
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
        let outcome = RequestOutcome::error(ResponseCode::FormErr);
        let attributes = outcome_attributes(&request, &outcome);
        assert_eq!(attribute(&attributes, "dns.response.code"), Some("FORMERR"));
    }

    /// Pins the exact strings the four attributes carry for the common
    /// question/transport/response-code combinations.
    #[test]
    fn common_attribute_values_are_pinned() {
        let cases = [
            (
                RecordType::A,
                Protocol::Udp,
                ResponseCode::NoError,
                ("A", "udp", "NOERROR"),
            ),
            (
                RecordType::AAAA,
                Protocol::Udp,
                ResponseCode::NoError,
                ("AAAA", "udp", "NOERROR"),
            ),
            (
                RecordType::A,
                Protocol::Tcp,
                ResponseCode::NXDomain,
                ("A", "tcp", "NXDOMAIN"),
            ),
            (
                RecordType::AAAA,
                Protocol::Tcp,
                ResponseCode::ServFail,
                ("AAAA", "tcp", "SERVFAIL"),
            ),
        ];

        for (
            query_type,
            protocol,
            response_code,
            (expected_type, expected_protocol, expected_code),
        ) in cases
        {
            let request = make_request_with(query_type, protocol);
            let mut outcome = RequestOutcome::success("native".into(), 1, None, None);
            outcome.response_code = response_code;

            let attributes = outcome_attributes(&request, &outcome);
            assert_eq!(
                attribute(&attributes, "dns.question.type"),
                Some(expected_type)
            );
            assert_eq!(
                attribute(&attributes, "network.protocol.name"),
                Some(expected_protocol)
            );
            assert_eq!(
                attribute(&attributes, "dns.response.code"),
                Some(expected_code)
            );
            assert_eq!(attribute(&attributes, "dns.authority"), Some("native"));
        }
    }

    /// Rare question types keep the derived `Debug` spelling, never a
    /// hand-written one.
    #[test]
    fn rare_question_types_keep_the_debug_spelling() {
        let cases = [
            (RecordType::ANY, "ANY"),
            (RecordType::OPT, "OPT"),
            (RecordType::Unknown(65000), "Unknown(65000)"),
            (RecordType::ZERO, "ZERO"),
        ];

        for (query_type, expected) in cases {
            let request = make_request_with(query_type, Protocol::Udp);
            let outcome = RequestOutcome::success("native".into(), 0, None, None);
            let attributes = outcome_attributes(&request, &outcome);
            assert_eq!(attribute(&attributes, "dns.question.type"), Some(expected));
        }
    }

    /// Rare response codes fall back to the upper-cased `Debug` spelling.
    #[test]
    fn rare_response_codes_keep_the_upper_case_debug_spelling() {
        let request = make_request();
        let cases = [
            // `ResponseCode` has an inherent `from(high, low)`, so name the
            // trait conversion for the numeric spelling.
            (<ResponseCode as From<u16>>::from(23), "BADCOOKIE"),
            (ResponseCode::Unknown(99), "UNKNOWN(99)"),
        ];

        for (response_code, expected) in cases {
            let outcome = RequestOutcome::error(response_code);
            let attributes = outcome_attributes(&request, &outcome);
            assert_eq!(attribute(&attributes, "dns.response.code"), Some(expected));
        }
    }

    /// A request that does not carry exactly one question has neither a
    /// question type nor a transport to report — `request_info()` rejects it as
    /// a unit, discarding the transport it could still see — so both fall back
    /// to `"unknown"`.
    #[test]
    fn questionless_requests_fall_back_to_unknown() {
        let message = Message::new(1, MessageType::Query, OpCode::Query);
        let bytes = message.to_vec().unwrap();
        let request = Request::from_bytes(
            bytes,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 5353)),
            Protocol::Tcp,
        )
        .unwrap();
        let outcome = RequestOutcome::error(ResponseCode::FormErr);

        let attributes = outcome_attributes(&request, &outcome);
        assert_eq!(attribute(&attributes, "dns.question.type"), Some("unknown"));
        assert_eq!(
            attribute(&attributes, "network.protocol.name"),
            Some("unknown")
        );
        assert_eq!(attribute(&attributes, "dns.response.code"), Some("FORMERR"));
        assert_eq!(attribute(&attributes, "dns.authority"), Some("none"));
    }

    #[test]
    fn xdp_counter_records_result_attributes() {
        use opentelemetry_sdk::metrics::InMemoryMetricExporter;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("test");
        let metrics = XdpMetrics::new(&meter);

        metrics.record("hit", 3);
        metrics.record("miss", 0);
        provider.force_flush().expect("flush metrics");

        let exported = exporter.get_finished_metrics().expect("metrics exported");
        let metric = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "dns.xdp.queries")
            .expect("dns.xdp.queries must be exported");

        assert_eq!(metric.unit(), "1", "dns.xdp.queries must carry unit \"1\"");

        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("dns.xdp.queries must be a u64 sum");
        };
        let points: Vec<_> = sum.data_points().collect();
        assert_eq!(points.len(), 1, "zero counts must not create a time series");
        assert_eq!(points[0].value(), 3);

        let attributes: Vec<KeyValue> = points[0].attributes().cloned().collect();
        assert_eq!(attribute(&attributes, "dns.xdp.result"), Some("hit"));
    }
}
