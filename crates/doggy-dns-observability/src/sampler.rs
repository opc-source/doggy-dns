use opentelemetry::trace::{Link, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{Sampler, SamplingDecision, SamplingResult, ShouldSample};

/// Name of the dedicated span emitted for `tracing::error!` events.
/// Must match the span created by `error_events::ErrorEventLayer`.
pub(crate) const ERROR_SPAN_NAME: &str = "dns.error";

/// Parent-based ratio sampling with one exception: spans named
/// [`ERROR_SPAN_NAME`] are always recorded and exported.
///
/// Normal traffic keeps the configured `sample_ratio`; every `tracing::error!`
/// event produces a forced-sampled `dns.error` child span, preserving the
/// original trace ID even when the request root was not sampled.
#[derive(Clone, Debug)]
pub(crate) struct ErrorBiasedSampler {
    ratio: f64,
}

impl ErrorBiasedSampler {
    pub(crate) fn new(ratio: f64) -> Self {
        Self { ratio }
    }
}

impl ShouldSample for ErrorBiasedSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[Link],
    ) -> SamplingResult {
        let parent_span = parent_context.map(|context| context.span().span_context().clone());
        let valid_parent = parent_span.as_ref().filter(|span| span.is_valid());
        let trace_state = valid_parent
            .map(|span| span.trace_state().clone())
            .unwrap_or_default();

        // Force-sample error spans even when their parent was not sampled, so
        // the error keeps the original trace ID.
        if name == ERROR_SPAN_NAME {
            return SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                trace_state,
            };
        }

        if let Some(parent) = valid_parent {
            return SamplingResult {
                decision: if parent.is_sampled() {
                    SamplingDecision::RecordAndSample
                } else {
                    SamplingDecision::Drop
                },
                attributes: Vec::new(),
                trace_state,
            };
        }

        Sampler::TraceIdRatioBased(self.ratio).should_sample(
            parent_context,
            trace_id,
            name,
            span_kind,
            attributes,
            links,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use opentelemetry::trace::TraceState;

    fn trace_id() -> TraceId {
        TraceId::from_hex("0123456789abcdef0123456789abcdef").unwrap()
    }

    fn sample(
        sampler: &ErrorBiasedSampler,
        name: &str,
        attributes: &[KeyValue],
    ) -> SamplingDecision {
        sampler
            .should_sample(None, trace_id(), name, &SpanKind::Server, attributes, &[])
            .decision
    }

    #[test]
    fn error_span_is_force_sampled_at_zero_ratio() {
        let sampler = ErrorBiasedSampler::new(0.0);
        assert_eq!(
            sample(&sampler, ERROR_SPAN_NAME, &[]),
            SamplingDecision::RecordAndSample
        );
    }

    #[test]
    fn normal_span_is_dropped_at_zero_ratio() {
        let sampler = ErrorBiasedSampler::new(0.0);
        assert_eq!(sample(&sampler, "dns.request", &[]), SamplingDecision::Drop);
    }

    fn parent_context(sampled: bool) -> Context {
        use opentelemetry::trace::{SpanContext, SpanId, TraceFlags};

        let span_context = SpanContext::new(
            trace_id(),
            SpanId::from_bytes([2; 8]),
            if sampled {
                TraceFlags::SAMPLED
            } else {
                TraceFlags::NOT_SAMPLED
            },
            true,
            TraceState::default(),
        );
        Context::new().with_remote_span_context(span_context)
    }

    #[test]
    fn error_span_is_forced_under_unsampled_parent() {
        let sampler = ErrorBiasedSampler::new(0.0);
        let parent = parent_context(false);
        let result = sampler.should_sample(
            Some(&parent),
            trace_id(),
            ERROR_SPAN_NAME,
            &SpanKind::Server,
            &[],
            &[],
        );
        assert_eq!(result.decision, SamplingDecision::RecordAndSample);
    }

    #[test]
    fn normal_span_is_dropped_under_unsampled_parent() {
        let sampler = ErrorBiasedSampler::new(1.0);
        let parent = parent_context(false);
        let result = sampler.should_sample(
            Some(&parent),
            trace_id(),
            "dns.request",
            &SpanKind::Server,
            &[],
            &[],
        );
        assert_eq!(result.decision, SamplingDecision::Drop);
    }

    #[test]
    fn normal_span_is_sampled_under_sampled_parent() {
        let sampler = ErrorBiasedSampler::new(0.0);
        let parent = parent_context(true);
        let result = sampler.should_sample(
            Some(&parent),
            trace_id(),
            "dns.request",
            &SpanKind::Server,
            &[],
            &[],
        );
        assert_eq!(result.decision, SamplingDecision::RecordAndSample);
    }

    #[test]
    fn normal_span_is_sampled_at_full_ratio() {
        let sampler = ErrorBiasedSampler::new(1.0);
        assert_eq!(
            sample(&sampler, "dns.request", &[]),
            SamplingDecision::RecordAndSample
        );
    }
}
