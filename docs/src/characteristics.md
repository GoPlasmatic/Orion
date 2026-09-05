<!-- description: The capability map: eight architectural characteristics, what the Orion runtime already provides under each, and the gaps it deliberately leaves open. -->
# Architectural Characteristics

Everything here is infrastructure the runtime already carries — not code you
write, and not per-service. This page is the map: each entry links to the page
that owns it in full.

Click a node to expand it; click a capability to open its page.

```orion-mindmap
{
  "centre": "Orion",
  "height": 680,
  "branches": [
    {
      "name": "Observability", "type": "observability",
      "children": [
        { "name": "Structured logging", "link": "operate/monitoring.html#structured-logging",
          "children": ["JSON or pretty output", "Five levels", "Per-crate RUST_LOG control", "x-request-id on every request"] },
        { "name": "Prometheus metrics", "link": "operate/monitoring.html#prometheus-metrics",
          "children": ["Request counts & error rates", "Latency histograms", "Engine & queue gauges", "Breaker state", "Rate-limit rejections"] },
        { "name": "Distributed tracing", "link": "operate/monitoring.html#distributed-tracing",
          "children": ["W3C trace context", "OTLP export", "Configurable sample rate", "One span per task"] },
        { "name": "Health endpoints", "link": "operate/monitoring.html#health-endpoints",
          "children": ["/healthz — liveness", "/readyz — readiness", "/health — component state", "Two-tier detail"] },
        { "name": "Execution traces", "link": "operate/traces.html#read-a-trace",
          "children": ["Per-task input, output, timing", "sync / async / batch / off", "Per-channel override"] }
      ]
    },
    {
      "name": "Resilience", "type": "infra",
      "children": [
        { "name": "Timeouts", "link": "operate/failure-handling.html#bound-how-long-anything-may-take",
          "children": ["Channel timeout", "Connector query & connect", "HTTP client backstop", "Per-ingress ceilings"] },
        { "name": "Retries", "link": "operate/failure-handling.html#retry-only-what-is-safe-to-retry",
          "children": ["HTTP connectors only", "Exponential backoff, capped 60s", "Idempotent methods only"] },
        { "name": "Circuit breakers", "link": "operate/failure-handling.html#stop-calling-a-failing-backend",
          "children": ["Per connector", "Cooldown before probing", "Off by default", "Inspect & reset via API"] },
        { "name": "Dead letter queue", "link": "operate/traces.html#drain-the-dead-letter-queue",
          "children": ["Failed async traces", "Automatic retry", "Kafka DLQ topic"] },
        { "name": "Graceful shutdown", "link": "operate/failure-handling.html#shut-down-without-dropping-requests",
          "children": ["SIGTERM / SIGINT", "Bounded drain window", "Force timeout"] },
        { "name": "Panic recovery", "link": "operate/failure-handling.html#what-survives-a-panic",
          "children": ["Request fails, process lives"] }
      ]
    },
    {
      "name": "Security", "type": "gateway",
      "children": [
        { "name": "Admin authentication", "link": "operate/security.html#authenticate-the-admin-plane",
          "children": ["Constant-time comparison", "sha256: digests", "Several keys for rotation", "Startup error in production"] },
        { "name": "Data-plane authentication", "link": "operate/security.html#decide-how-the-data-plane-authenticates",
          "children": ["api_key per channel", "hmac — templates & presets", "jwt with claims in context", "Uniform 401", "No OIDC flows or mTLS"] },
        { "name": "Secrets by reference", "link": "reference/connectors.html#secrets-by-reference",
          "children": ["env:// and vault://", "Masked in API reads", "Optional AES-256-GCM at rest"] },
        { "name": "Payload validation", "link": "reference/channel-config.html#validation",
          "children": ["JSONLogic rules per channel", "Body size limit", "Runs before any logic"] },
        { "name": "Egress control", "link": "operate/security.html#bound-what-connectors-can-reach",
          "children": ["SSRF private-IP blocking", "Per-connector opt-in", "Operation gates"] },
        { "name": "Query safety", "link": "reference/connectors.html#dialect-guards",
          "children": ["Parameterized queries", "Dialect guards on db"] },
        { "name": "Transport & headers", "link": "operate/security.html#terminate-tls",
          "children": ["TLS termination", "CSP, X-Frame-Options, HSTS", "Server-side origin checks"] }
      ]
    },
    {
      "name": "Scalability", "type": "ci",
      "children": [
        { "name": "Rate limiting", "link": "reference/channel-config.html#rate-limiting",
          "children": ["Token bucket with burst", "JSONLogic client key", "Per-channel and platform-wide", "429 on an empty bucket"] },
        { "name": "Backpressure", "link": "reference/channel-config.html#backpressure",
          "children": ["Per-channel semaphore", "503 load shedding", "Shared across ingresses"] },
        { "name": "Deduplication", "link": "reference/channel-config.html#deduplication",
          "children": ["Idempotency key window", "HTTP and Kafka", "Shared store when clustered"] },
        { "name": "Async processing", "link": "operate/traces.html#run-an-async-channel",
          "children": ["Trace id returned at once", "Bounded worker queue", "DLQ behind it"] },
        { "name": "Cluster mode", "link": "operate/cluster.html#what-the-cluster-shares",
          "children": ["N replicas, one system", "Shared limits, dedup, caches", "Epoch config propagation", "Leased scheduled jobs"] },
        { "name": "Database backends", "link": "reference/configuration.html#database-backend",
          "children": ["SQLite embedded", "PostgreSQL", "MySQL", "Chosen by URL scheme"] }
      ]
    },
    {
      "name": "Deployability", "type": "service",
      "children": [
        { "name": "Single binary", "link": "getting-started/install.html#install-the-server",
          "children": ["Embedded database", "Homebrew tap", "Shell & PowerShell installers", "Multi-platform binaries"] },
        { "name": "Containers", "link": "operate/docker.html",
          "children": ["Multi-stage image", "Non-root execution", "Health probes", "Reference HA compose"] },
        { "name": "Kubernetes", "link": "operate/kubernetes.html",
          "children": ["Helm chart", "Liveness & readiness probes", "Migrations as a Job", "Cluster mode via values"] },
        { "name": "Configuration", "link": "reference/configuration.html#how-settings-are-resolved",
          "children": ["TOML file", "ORION_SECTION__KEY overrides", "Defaults for everything", "Misspelling is a startup error"] },
        { "name": "Pre-flight checks", "link": "build/testing.html#check-the-config-and-the-backends",
          "children": ["validate-config", "test-connectivity", "preflight"] }
      ]
    },
    {
      "name": "Extensibility", "type": "connector",
      "children": [
        { "name": "Connector types", "link": "reference/connectors.html",
          "children": ["http", "kafka", "db — PG / MySQL / SQLite / Mongo", "cache — Redis or memory", "es", "smtp — transactional email", "storage — S3-compatible"] },
        { "name": "Task functions", "link": "reference/functions.html",
          "children": ["Parse, map, filter, validate", "HTTP calls", "Portable dialect & raw SQL", "Cache read and write", "Kafka publish"] },
        { "name": "Portable data dialect", "link": "reference/data-dialect.html",
          "children": ["One filter syntax", "Lowered to SQL", "Lowered to MongoDB", "Lowered to Query DSL"] },
        { "name": "Channel protocols", "link": "reference/channel-config.html#routing--protocol",
          "children": ["REST with route patterns", "Plain HTTP by name", "Kafka topics", "Sync or async"] },
        { "name": "In-process composition", "link": "reference/functions.html#channel_call",
          "children": ["No network hop", "No serialization", "Callee guards still apply"] }
      ]
    },
    {
      "name": "Availability", "type": "workflow",
      "children": [
        { "name": "Hot reload", "link": "concepts/lifecycle.html#what-moves-the-engine",
          "children": ["Atomic engine swap", "In-flight requests unaffected", "No restart"] },
        { "name": "Immutable versions", "link": "concepts/lifecycle.html#why-immutability-is-the-load-bearing-rule",
          "children": ["Active cannot be edited", "Change is a new version", "History kept"] },
        { "name": "Canary rollout", "link": "build/versioning.html#roll-out-gradually",
          "children": ["Percentage traffic split", "Sticky per caller", "Moved with one call"] },
        { "name": "Rollback", "link": "build/versioning.html#roll-back",
          "children": ["Rollout to 0", "Or archive the version", "Nothing to redeploy"] },
        { "name": "Response caching", "link": "reference/channel-config.html#response-caching",
          "children": ["Keyed from the request", "Memory or Redis", "Never reaches the workflow"] },
        { "name": "Connection pooling", "link": "reference/connectors.html#db",
          "children": ["Pools cached per connector", "Reused across requests"] }
      ]
    },
    {
      "name": "Maintainability", "type": "channel",
      "children": [
        { "name": "Admin API", "link": "reference/admin-api.html",
          "children": ["CRUD for every entity", "Lifecycle transitions", "Engine control", "Dependency inspection"] },
        { "name": "OpenAPI", "link": "reference/openapi.html",
          "children": ["Full admin surface", "Swagger UI outside production", "Closed in production"] },
        { "name": "Offline testing", "link": "build/testing.html",
          "children": ["Lint a workflow file", "Dry-run with stubs", "*.case.json suites", "No server needed"] },
        { "name": "Packages & promotion", "link": "operate/promotion.html",
          "children": ["export, lint, plan, apply, diff", "Content-immutable versions", "Receipts of what shipped"] },
        { "name": "CI/CD", "link": "guides/ci-cd.html",
          "children": ["Offline PR gates", "Plan-then-apply pipelines", "Scheduled drift detection"] },
        { "name": "Audit logs", "link": "operate/audit-logs.html",
          "children": ["Who changed what, when", "Filterable", "Bounded retention", "Change-context grouping"] },
        { "name": "Backups", "link": "operate/backup-restore.html",
          "children": ["SQLite backup via API", "Offline restore procedure", "PG/MySQL snapshot tooling"] }
      ]
    }
  ]
}
```

## Observability

| Area | What the runtime provides |
|---|---|
| [Structured logging](./operate/monitoring.md#structured-logging) | JSON or pretty output, five levels, per-crate control through `RUST_LOG`. Every request carries an `x-request-id` — sent by you or generated — that appears in the logs and comes back on the response. |
| [Prometheus metrics](./operate/monitoring.md#prometheus-metrics) | Request counts and error rates, latency histograms, engine and trace-queue gauges, breaker state, rate-limit rejections. Served on a dedicated bind address; every series is named in the [Metrics Reference](./reference/metrics.md). |
| [Distributed tracing](./operate/monitoring.md#distributed-tracing) | W3C trace context accepted and propagated, OTLP export to a collector, configurable sample rate, one span per task. |
| [Health endpoints](./operate/monitoring.md#health-endpoints) | `/healthz` for liveness, `/readyz` for readiness, `/health` for per-component state. `/health` is two-tier: anonymous callers see coarse status, admin callers see the detail. |
| [Execution traces](./operate/traces.md#read-a-trace) | Each task's input, output and timing, stored per request. Persistence is `sync`, `async`, `batch` or `off`, set globally and overridable per channel. |

## Resilience

| Area | What the runtime provides |
|---|---|
| [Timeouts](./operate/failure-handling.md#bound-how-long-anything-may-take) | Four levels, outside in: channel, connector, HTTP client, health check. The channel timeout applies on every ingress — sync, `/async`, Kafka and `channel_call` — with per-ingress ceilings that clamp it. |
| [Retries](./operate/failure-handling.md#retry-only-what-is-safe-to-retry) | HTTP connectors only: exponential backoff capped at 60 s, on retryable failures of idempotent methods. Nothing else is re-driven, because a timed-out `INSERT` may already have applied. |
| [Circuit breakers](./operate/failure-handling.md#stop-calling-a-failing-backend) | Per connector, isolated from each other, with a cooldown before probing again. Off by default; inspectable and resettable through the admin API. |
| [Dead letter queue](./operate/traces.md#drain-the-dead-letter-queue) | Async traces that fail land in the DLQ and are retried from there. Kafka gets its own DLQ topic so one poison record cannot stall a partition. |
| [Graceful shutdown](./operate/failure-handling.md#shut-down-without-dropping-requests) | `SIGTERM`/`SIGINT` stop new admissions and drain in-flight work within a bounded window before the process exits. |
| [Panic recovery](./operate/failure-handling.md#what-survives-a-panic) | A panicking task fails its own request. The process keeps serving. |

## Security

| Area | What the runtime provides |
|---|---|
| [Admin authentication](./operate/security.md#authenticate-the-admin-plane) | Keys compared in constant time, storable as `sha256:` digests, several at once so rotation needs no downtime. Missing admin auth is a startup error in production, not a warning. |
| [Data-plane authentication](./operate/security.md#decide-how-the-data-plane-authenticates) | Per channel: `api_key`, `hmac` over a templated signing string (raw body by default; provider presets) verified before parsing, or `jwt` with the verified claims exposed to channel logic and the workflow. OIDC flows and mTLS termination stay out of scope — put a gateway in front for those. |
| [Secrets by reference](./reference/connectors.md#secrets-by-reference) | `env://` and `vault://` references resolved at load, never stored inline. Secret fields are masked in API reads, and the stored config is AES-256-GCM encrypted at rest when `storage.connector_encryption_key` is set (plaintext by default). |
| [Payload validation](./reference/channel-config.md#validation) | JSONLogic rules per channel, plus a body-size limit, enforced before any workflow logic runs. |
| [Egress control](./operate/security.md#bound-what-connectors-can-reach) | SSRF protection blocks private and internal addresses unless a connector opts in. [Operation gates](./reference/connectors.md#operation-gates) make a connector read-only, delete-proof, or anything between. |
| [Query safety](./reference/connectors.md#dialect-guards) | Parameterized queries throughout, with dialect guards on `db` connectors that refuse statements the connector was not opened for. |
| [Transport, headers and origins](./operate/security.md#terminate-tls) | TLS termination in-process or at a proxy; CSP, `X-Frame-Options`, HSTS and the rest set by default; [origin checks](./operate/security.md#check-origins-server-side) enforced server-side rather than trusted from the browser. |

## Scalability

| Area | What the runtime provides |
|---|---|
| [Rate limiting](./reference/channel-config.md#rate-limiting) | Token bucket with burst, per channel, keyed by a JSONLogic expression over the request — so a limit can be per API key, per tenant, or per IP. A platform-wide limit sits above it. |
| [Backpressure](./reference/channel-config.md#backpressure) | A per-channel semaphore bounds in-flight work. When permits run out the channel sheds load with `503` immediately rather than queueing. |
| [Deduplication](./reference/channel-config.md#deduplication) | Idempotency keys refused within a window, on HTTP and on Kafka, backed by a shared store when clustered. |
| [Async processing](./operate/traces.md#run-an-async-channel) | `/async` returns a trace id at once and a bounded worker queue drains the work, with the DLQ behind it. |
| [Cluster mode](./operate/cluster.md#what-the-cluster-shares) | N replicas as one logical system: rate limits, deduplication and caches shared through Redis, config changes propagated by epoch, scheduled jobs leased to one node. |
| [Database backends](./reference/configuration.md#database-backend) | SQLite embedded for a single node; PostgreSQL or MySQL when replicas share state. The scheme in `storage.url` picks one — nothing else changes. |

## Deployability

| Area | What the runtime provides |
|---|---|
| [Single binary](./getting-started/install.md#install-the-server) | One executable with an embedded database and no runtime to install beside it. Homebrew, shell and PowerShell installers, prebuilt multi-platform binaries, or from source. |
| [Containers](./operate/docker.md) | Multi-stage image, non-root execution, health probes wired up, and a reference HA compose topology. |
| [Kubernetes](./operate/kubernetes.md) | A Helm chart with liveness and readiness probes, migrations as a Job rather than at boot, and cluster mode configured through values. |
| [Configuration](./reference/configuration.md#how-settings-are-resolved) | TOML with `ORION_SECTION__KEY` environment overrides and a default for everything. A misspelled key is a startup error, not a silent no-op. |
| [Pre-flight checks](./build/testing.md#check-the-config-and-the-backends) | `validate-config`, `test-connectivity` and `preflight` answer "will this instance come up, reach its backends, and load what is already stored" before the deploy, not after. |

## Extensibility

| Area | What the runtime provides |
|---|---|
| [Connector types](./reference/connectors.md) | `http`, `kafka`, `db` (PostgreSQL, MySQL, SQLite, MongoDB — chosen by connection-string scheme), `cache` (Redis or in-memory), `es`, `smtp` (transactional email), and `storage` (S3-compatible object stores). |
| [Task functions](./reference/functions.md) | Parsing, mapping, filtering, validation and logging; HTTP calls; the portable data dialect and raw SQL; cache read and write; Kafka publish. |
| [Portable data dialect](./reference/data-dialect.md) | One filter-and-envelope syntax lowered to SQL, MongoDB, or an Elasticsearch Query DSL body, so the same task reads from any of them. |
| [Channel protocols](./reference/channel-config.md#routing--protocol) | REST with route patterns and path parameters, plain HTTP by channel name, and Kafka topics — each of them sync or async where the transport allows. |
| [In-process composition](./reference/functions.md#channel_call) | `channel_call` runs another channel's workflow in the same process: no network hop, no serialization, and the callee's guards still apply. |

| [Plugins](./reference/plugins.md) | Custom task functions as sandboxed WebAssembly components: a pure JSON → JSON transformation, uploaded and activated like any other definition, promoted in packages, synced across a cluster. The sandbox imports nothing — no filesystem, clock, network or secrets. |

> [!NOTE]
> Orion extends **by configuration, and by pure code in a sandbox**. A plugin
> is the one way to add a task function at runtime, and it can only compute:
> everything with I/O stays a connector or a service of your own.
> [What you can extend](./concepts/how-orion-works.md#what-you-can-extend)
> states the boundary exactly.

## Availability

| Area | What the runtime provides |
|---|---|
| [Hot reload](./concepts/lifecycle.md#what-moves-the-engine) | Activating a version swaps the engine atomically. Requests already in flight finish on the engine they started on. No restart, no dropped connection, no deploy window. |
| [Immutable versions](./concepts/lifecycle.md#why-immutability-is-the-load-bearing-rule) | An active version cannot be edited. Every change is a new version, so what ran yesterday is still there to compare against and return to. |
| [Canary rollout](./build/versioning.md#roll-out-gradually) | A percentage of traffic on the new version, sticky per caller by header, moved up or back with one call. |
| [Rollback](./build/versioning.md#roll-back) | Setting the rollout to `0` or archiving the new version restores the previous one at once — no rebuild, and nothing to redeploy. |
| [Response caching](./reference/channel-config.md#response-caching) | Per channel, keyed from the request, in memory or in Redis, so repeated reads never reach the workflow. |
| [Connection pooling](./reference/connectors.md#db) | Pools are cached per connector and reused across requests instead of being opened per call. |

## Maintainability

| Area | What the runtime provides |
|---|---|
| [Admin API](./reference/admin-api.md) | Full CRUD over channels, workflows, connectors and packages, plus lifecycle transitions, engine control, and dependency inspection. Everything the console and the CLI do, they do through it. |
| [OpenAPI](./reference/openapi.md) | The whole admin surface as a spec, with Swagger UI served outside production and closed off inside it. |
| [Offline testing](./build/testing.md) | Lint a workflow file, dry-run it against sample input with stubbed connectors, and keep `*.case.json` regression suites next to the JSON. None of it needs a running server. |
| [Packages and promotion](./operate/promotion.md) | `export`, `lint`, `plan`, `apply` and `diff` move a named, versioned unit between instances. Applied versions are content-immutable, and receipts record exactly what shipped. |
| [CI/CD](./guides/ci-cd.md) | Offline pull-request gates, plan-then-apply pipelines, scheduled drift detection, and a rollback path that is the same one verb. |
| [Audit logs](./operate/audit-logs.md) | Who changed what and when, filterable, with bounded retention and a change-context header that groups a multi-step operation into one story. |
| [Backups](./operate/backup-restore.md) | SQLite backups on demand through the API. Restore is a deliberate offline procedure, and PostgreSQL or MySQL use your own snapshot and PITR tooling. |

## Related

- [How Orion Works](./concepts/how-orion-works.md): the three primitives and one
  request's journey, if this page is your first stop.
- [Production Checklist](./operate/production-checklist.md): the same estate as
  a list of things to set before go-live, with the owning page for each.
- [Configuration Reference](./reference/configuration.md): every setting behind
  the characteristics above.
- [Design Notes](./reference/design-notes.md): why several of these are built
  the way they are.
