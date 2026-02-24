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
                .build(),
        )
        .build();

    let tracer = provider.tracer("orion");

    // Register the global tracer provider so outbound propagation works
    opentelemetry::global::set_tracer_provider(provider.clone());

    Ok((provider, tracer))
}
