use std::collections::HashMap;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Cached propagator — `TraceContextPropagator` is stateless, so a single
/// instance can be shared across all requests instead of allocating per-request.
static PROPAGATOR: LazyLock<TraceContextPropagator> = LazyLock::new(TraceContextPropagator::new);

/// A simple extractor that pulls header values from an HTTP request.
struct HeaderExtractor<'a> {
    headers: &'a axum::http::HeaderMap,
}

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.keys().map(|k| k.as_str()).collect()
    }
}

/// Axum middleware that extracts W3C Trace Context (`traceparent`/`tracestate`)
/// from inbound HTTP requests and sets the extracted context as the parent of
/// a new span.
///
/// When a calling service sends a `traceparent` header, this middleware ensures
/// that Orion's spans appear as children in the caller's distributed trace.
pub async fn extract_trace_context(req: Request<Body>, next: Next) -> Response {
    let extractor = HeaderExtractor {
        headers: req.headers(),
    };
    let parent_cx = PROPAGATOR.extract(&extractor);

    // Build span with trace_id/span_id fields for log correlation
    let span = {
        use opentelemetry::trace::TraceContextExt;
        let span_ref = parent_cx.span();
        let sc = span_ref.span_context();
        if sc.is_valid() {
            tracing::info_span!(
                "http_request",
                trace_id = %sc.trace_id(),
                span_id = %sc.span_id(),
            )
        } else {
            tracing::info_span!("http_request")
        }
    };

    // Set the extracted context as the parent of the current span
    span.set_parent(parent_cx);

    // Run the rest of the middleware/handler inside this span
    let _guard = span.enter();
    next.run(req).await
}

/// Inject the current span's trace context into a header map.
///
/// Call this from any code that makes outbound requests (HTTP, Kafka, trace queue)
/// to propagate the trace to downstream services or background processing.
pub fn inject_trace_context(headers: &mut HashMap<String, String>) {
    struct MapInjector<'a> {
        headers: &'a mut HashMap<String, String>,
    }

    impl opentelemetry::propagation::Injector for MapInjector<'_> {
        fn set(&mut self, key: &str, value: String) {
            self.headers.insert(key.to_string(), value);
        }
    }

    let cx = Span::current().context();
    PROPAGATOR.inject_context(&cx, &mut MapInjector { headers });
}
