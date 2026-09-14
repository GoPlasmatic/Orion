<!-- description: The observability settings of orion-server: log level and format, the Prometheus metrics listener, and OpenTelemetry span export. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Observability settings

Logging, metrics and OpenTelemetry export. Every table on these pages carries the wire name, the default the code uses, and the `ORION_*` override.

| Page | Holds |
|---|---|
| [Logging and metrics settings](./logging-metrics.md) | log level and format, enabling Prometheus metrics, and the dedicated unauthenticated metrics listener. |
| [Tracing settings](./tracing.md) | OpenTelemetry span export, the OTLP endpoint, service name, sample rate and the debug profiling switch. |

## Related

- [Server configuration](./index.md): every section, by what you are configuring.
- [How settings are resolved](./how-settings-are-resolved.md): defaults, the file, and the environment.
- [Production checklist](../../operate/production-checklist.md): which settings to change before real traffic.
