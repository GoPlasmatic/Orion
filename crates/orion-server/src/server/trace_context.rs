//! The HTTP half of trace-context propagation: the axum middleware that reads
//! `traceparent` off an inbound request.
//!
//! The transport-neutral map helpers — what Kafka, the async trace queue and
//! `http_call` use — are in [`crate::trace_context`]. They moved down because
//! three of the four callers are not the HTTP layer, and a workflow function
//! handler reaching up into `server` to propagate a header was the giveaway.

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::propagation::TextMapPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::trace_context::PROPAGATOR;

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
    let _ = span.set_parent(parent_cx);

    // Run the rest of the middleware/handler inside this span
    let _guard = span.enter();
    next.run(req).await
}
