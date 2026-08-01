use crate::config::TracingConfig;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

/// Initialize the OpenTelemetry tracing pipeline.
///
/// Sets up an OTLP gRPC exporter, a batch span processor, and configures W3C
/// Trace Context propagation. Returns the tracer provider for graceful
/// shutdown and a tracer for creating the `tracing_opentelemetry` layer.
///
/// Usage in main.rs:
/// ```ignore
/// let (provider, tracer) = init_otel_pipeline(&config.tracing)?;
/// let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
/// registry.with(otel_layer).init();
/// ```
pub fn init_otel_pipeline(
    config: &TracingConfig,
    instance_id: &str,
) -> Result<(SdkTracerProvider, opentelemetry_sdk::trace::Tracer), Box<dyn std::error::Error>> {
    // Set the global text map propagator to W3C Trace Context (traceparent / tracestate)
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let sampler = if config.sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if config.sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(config.sample_rate)
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.otlp_endpoint)
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(config.service_name.clone())
                // Standard semconv attribute — distinguishes replicas of the
                // same service in trace backends (multi-instance C2).
                .with_attribute(opentelemetry::KeyValue::new(
                    "service.instance.id",
                    instance_id.to_string(),
                ))
                .build(),
        )
        .build();

    let tracer = provider.tracer("orion");

    // Register the global tracer provider so outbound propagation works
    opentelemetry::global::set_tracer_provider(provider.clone());

    Ok((provider, tracer))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T35: nothing else exercises this pipeline — the integration harness
    /// forbids `init_observability` (global subscriber), so a dependency
    /// bump breaking exporter construction would first surface at production
    /// startup with `tracing.enabled`. Construction is lazy (the OTLP tonic
    /// exporter does not connect until spans flush), so this runs offline:
    /// it proves the exporter builds, all three sampler regimes construct,
    /// and shutdown of a never-exported provider completes. A runtime must
    /// be live: the tonic exporter binds its channel to the reactor at
    /// construction even though nothing connects.
    #[tokio::test]
    async fn pipeline_builds_offline_for_every_sampler_regime() {
        for sample_rate in [0.0, 0.5, 1.0] {
            let config = TracingConfig {
                enabled: true,
                otlp_endpoint: "http://127.0.0.1:1".to_string(),
                sample_rate,
                ..Default::default()
            };
            let (provider, _tracer) = init_otel_pipeline(&config, "test-instance")
                .expect("offline pipeline construction must succeed");
            let _ = provider.shutdown();
        }
    }
}
