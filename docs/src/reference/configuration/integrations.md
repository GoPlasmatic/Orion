<!-- description: The integration settings of orion-server: Kafka consumption and authentication, the WebAssembly plugin sandbox, and ONNX model admission and runtimes. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Integration settings

Kafka, WebAssembly plugins and ONNX models. Every table on these pages carries the wire name, the default the code uses, and the `ORION_*` override.

| Page | Holds |
|---|---|
| [Kafka settings](./kafka.md) | brokers, group id, topic mappings, timeouts, the dead-letter topic, SASL and TLS broker authentication and raw librdkafka properties. |
| [Plugin settings](./plugins.md) | enabling the WebAssembly sandbox, the memory, size, time and concurrency ceilings, trust keys and per-plugin overrides. |
| [Model settings](./models.md) | enabling ONNX models, the artifact cache, admission limits, inference ceilings, trust keys, runtimes, devices and overrides. |

## Related

- [Server configuration](./index.md): every section, by what you are configuring.
- [How settings are resolved](./how-settings-are-resolved.md): defaults, the file, and the environment.
- [Production checklist](../../operate/production-checklist.md): which settings to change before real traffic.
