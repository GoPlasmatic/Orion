//! W3C Trace Context propagation over plain string maps.
//!
//! Distributed tracing crosses every transport Orion has: an inbound HTTP
//! request carries a `traceparent`, an outbound `http_call` must pass it on, a
//! Kafka record carries it in its headers, and an async trace queue carries it
//! from the submitting request to the worker that runs the workflow. So the
//! four modules that need it — `server`, `engine::functions`, `kafka` and
//! `queue` — sit at four different layers.
//!
//! It used to live entirely in `server::trace_context`, which made a workflow
//! function handler reach up into the HTTP layer to propagate a trace header.
//! The map helpers are here, below all four; the axum middleware that reads a
//! request's headers stays in [`crate::server::trace_context`], because that
//! one really is about HTTP.

use std::collections::HashMap;
use std::sync::LazyLock;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Cached propagator — `TraceContextPropagator` is stateless, so a single
/// instance can be shared across all requests instead of allocating per-request.
pub(crate) static PROPAGATOR: LazyLock<TraceContextPropagator> =
    LazyLock::new(TraceContextPropagator::new);

/// [`opentelemetry::propagation::Extractor`] over a plain string map — the
/// shape Kafka headers and queued trace headers arrive in.
struct MapExtractor<'a>(&'a HashMap<String, String>);

impl opentelemetry::propagation::Extractor for MapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|v| v.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Extract a W3C trace context from a string header map and attach it as the
/// parent of the current tracing span. Returns the propagated context so the
/// caller can keep it in scope. Shared by the Kafka consumer and the async
/// trace queue; uses the cached `PROPAGATOR` instead of building one per
/// message.
pub fn set_parent_from_map(headers: &HashMap<String, String>) -> opentelemetry::Context {
    let cx = PROPAGATOR.extract(&MapExtractor(headers));
    let _ = Span::current().set_parent(cx.clone());
    cx
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip every transport relies on: what `inject` writes,
    /// `set_parent_from_map` reads back. Without a recording subscriber there
    /// is no active span context to propagate, so this asserts the shape —
    /// injection writes nothing when there is nothing to write, and extraction
    /// of an absent header yields an invalid context rather than a panic.
    #[test]
    fn an_absent_trace_context_is_not_invented() {
        use opentelemetry::trace::TraceContextExt;

        let mut headers = HashMap::new();
        inject_trace_context(&mut headers);
        assert!(
            !headers.contains_key("traceparent"),
            "no active trace must not produce a traceparent: {headers:?}"
        );

        let cx = set_parent_from_map(&HashMap::new());
        assert!(!cx.span().span_context().is_valid());
    }

    /// A `traceparent` a caller sent is what the next hop sees.
    #[test]
    fn a_caller_supplied_traceparent_is_propagated() {
        use opentelemetry::trace::TraceContextExt;

        let mut headers = HashMap::new();
        headers.insert(
            "traceparent".to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
        );

        let cx = set_parent_from_map(&headers);
        let span = cx.span();
        let sc = span.span_context();
        assert!(sc.is_valid());
        assert_eq!(
            sc.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(sc.span_id().to_string(), "00f067aa0ba902b7");
    }
}
