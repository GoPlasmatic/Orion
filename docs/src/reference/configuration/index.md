<!-- description: Every Orion server setting with its real default and ORION_* environment variable, grouped by what you are configuring, one page per config section. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Server configuration

Every setting Orion has, with its real default and its environment variable, one page per config section. `orion-server` with no config file starts and works; these pages are what you change for anything beyond a single-node development instance. A ready-to-edit file carrying the same values is [`config.toml.example`](https://github.com/GoPlasmatic/Orion/blob/main/crates/orion-server/config.toml.example), which the Docker image ships at `/app/config.toml.example`.

| Configure… | Page |
|---|---|
| Resolution, files, and validation | [How settings are resolved](./how-settings-are-resolved.md) |
| Production strictness | [Deployment environment](./environment.md) |
| Environment-specific values and secrets | [Vars and secrets](./vars-and-secrets.md) |
| HTTP, TLS, compression, and API docs | [Server settings](./server.md) |
| SQLite, PostgreSQL, or MySQL | [Storage settings](./storage.md) |
| Multiple replicas and Redis | [Cluster settings](./cluster.md) |
| Engine reload, connector failure, and the circuit breaker | [Engine settings](./engine.md) |
| Data-plane body size | [Ingest settings](./ingest.md) |
| Async traces and the dead-letter queue | [Trace queue settings](./trace-queue.md) |
| How traces are persisted | [Trace persistence settings](./trace-storage.md) |
| Audit retention | [Audit log settings](./audit.md) |
| Query safety bounds | [Query and write bounds](./query.md) |
| Scheduled (cron) channels | [Cron scheduler settings](./cron.md) |
| Packages a node applies to itself at startup | [Package settings](./packages.md) |
| Kafka ingestion and broker auth | [Kafka settings](./kafka.md) |
| Custom task functions in WebAssembly | [Plugin settings](./plugins.md) |
| ONNX models and the artifact cache | [Model settings](./models.md) |
| Admin API keys | [Admin authentication settings](./admin-auth.md) |
| JWKS egress policy | [JWT verification settings](./jwt.md) |
| OAuth2 token-endpoint egress policy | [Inbound OAuth2 sign-in settings](./oauth2-login.md) |
| Browser origins | [CORS settings](./cors.md) |
| Platform rate limits and trusted proxies | [Rate limit settings](./rate-limit.md) |
| Which channels a node serves | [Channel filter settings](./channel-filter.md) |
| Logs and Prometheus metrics | [Logging and metrics settings](./logging-metrics.md) |
| OpenTelemetry export | [Tracing settings](./tracing.md) |

## Related

- [Production checklist](../../operate/production-checklist.md): which of these settings to change before an instance takes real traffic.
- [Secure an instance](../../operate/run/security.md): the security settings in context, with the reasoning.
- [`orion-server` commands](../cli/orion-server/index.md): `validate-config`, `migrate`, and the rest.
- [Environment variables](../environment-variables.md): every way the process environment reaches a setting.
