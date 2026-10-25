use crate::sampler::ERROR_SPAN_NAME;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Turns every `tracing::error!` event into a force-sampled `dns.error` span.
///
/// The span is a child of the current span, so it keeps the request's trace ID
/// even when the parent was dropped by head sampling. The sampler force-samples
/// this span name. Normal traffic still uses `sample_ratio`; only ERROR events
/// get the extra span. If the event occurs outside any span, it becomes a root.
///
/// OTel SDK internal ERROR logs are ignored to avoid an export-failure feedback
/// loop.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ErrorEventLayer;

impl<S: Subscriber> Layer<S> for ErrorEventLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.level() != &Level::ERROR {
            return;
        }
        let target = metadata.target();
        if target.starts_with("opentelemetry") || target.starts_with("tracing_opentelemetry") {
            return;
        }

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.unwrap_or_else(|| "error event".to_string());

        let span = tracing::error_span!(
            ERROR_SPAN_NAME,
            otel.kind = "server",
            otel.status_code = "ERROR",
            "doggy.force_sample" = true,
            "error.message" = %message,
            "error.target" = %target,
            "code.filepath" = tracing::field::Empty,
            "code.lineno" = tracing::field::Empty,
        );
        if let Some(file) = metadata.file() {
            span.record("code.filepath", tracing::field::display(file));
        }
        if let Some(line) = metadata.line() {
            span.record("code.lineno", u64::from(line));
        }
        drop(span);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}
