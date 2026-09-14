<!-- description: The [tracing] settings: OpenTelemetry span export, the OTLP endpoint, service name, sample rate and the debug profiling switch. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Tracing settings

OpenTelemetry export, compiled into every binary and gated at runtime.

## Synopsis

```toml
[tracing]
enabled = false
otlp_endpoint = "http://localhost:4317"
service_name = "orion"
sample_rate = 1.0
debug_profile_enabled = false
```

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `tracing.enabled` | `false` | `ORION_TRACING__ENABLED` | Enable to export spans to an OTLP collector. |
| `tracing.otlp_endpoint` | `"http://localhost:4317"` | `ORION_TRACING__OTLP_ENDPOINT` | Point at your collector (Jaeger, Tempo, OTel Collector). |
| `tracing.service_name` | `"orion"` | `ORION_TRACING__SERVICE_NAME` | Distinguish multiple Orion deployments in one backend. |
| `tracing.sample_rate` | `1.0` | `ORION_TRACING__SAMPLE_RATE` | Lower under high traffic; `0.0` to `1.0`. |
| `tracing.debug_profile_enabled` | `false` | `ORION_TRACING__DEBUG_PROFILE_ENABLED` | Leave off in production. |

With `debug_profile_enabled = true`, a request carrying `X-Orion-Profile: 1` (or `?profile=1`) gets an `_orion.profile` object breaking the request down by phase. The phases are engine lock wait, per-handler durations, trace store, and residual workflow logic. It is off by default so callers cannot probe internal timing.

## Related

- [Monitor and alert](../../operate/run/monitoring.md): wiring the OTLP export to a collector.
- [Data API › Per-request profiling](../data-api.md#per-request-profiling): what `debug_profile_enabled` unlocks.
- [Trace persistence settings](./trace-storage.md): Orion's own trace records, which this export is not.
- [Server configuration](./index.md): every section, by what you are configuring.
