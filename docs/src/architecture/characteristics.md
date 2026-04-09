# Architectural Characteristics

Orion provides production-grade capabilities across eight architectural dimensions. Each subcategory below links to its detailed documentation.

> **C** Creational · **S** Structural · **B** Behavioral

---

## Observability — S

| Area | Capabilities |
|------|-------------|
| [Structured Logging](../features/observability.md#structured-logging) | JSON & pretty-print formats · Configurable log levels · Per-request context · Per-crate filtering |
| [Prometheus Metrics](../features/observability.md#prometheus-metrics) | Request counters & error rates · Latency histograms · Circuit breaker metrics · Rate limit rejections |
| [Distributed Tracing](../features/observability.md#distributed-tracing) | W3C Trace Context · OpenTelemetry OTLP export · Configurable sampling rate · Per-task span tracking |
| [Health Monitoring](../features/observability.md#health-monitoring) | Component-level health checks · Automatic degradation · Request ID propagation · Kubernetes liveness & readiness probes |

## Resilience — S

| Area | Capabilities |
|------|-------------|
| [Circuit Breakers](../features/resilience.md#circuit-breakers) | Lock-free state machine · Per-connector isolation · Auto-recovery after cooldown · Admin API to inspect & reset |
| [Retry & Backoff](../features/resilience.md#retry-backoff) | Exponential backoff (capped 60 s) · Configurable max retries · Retryable error detection |
| [Timeouts](../features/resilience.md#timeouts) | Per-channel enforcement · Workflow execution limits · Per-connector query timeout |
| [Fault Tolerance](../features/resilience.md#fault-tolerance) | Graceful shutdown (SIGTERM/SIGINT) · Connection draining · Dead letter queue with retry · Panic recovery middleware |

## Security — B

| Area | Capabilities |
|------|-------------|
| [Secret Management](../features/security.md#secret-management) | Auto-masked API responses · Credential isolation via connectors |
| [Input Validation](../features/security.md#input-validation) | Per-channel JSONLogic rules · Payload size limits · Header & query param access |
| [Network Security](../features/security.md#network-security) | SSRF protection (private IP blocking) · TLS/HTTPS support · Security headers (CSP, X-Frame-Options) |
| [Access Control](../features/security.md#access-control) | Admin API authentication · Per-channel CORS enforcement · Origin allowlist |
| [Data Safety](../features/security.md#data-safety) | Parameterized SQL queries · Injection protection · URL validation |

## Scalability — C

| Area | Capabilities |
|------|-------------|
| [Rate Limiting](../features/scalability.md#rate-limiting) | Token bucket algorithm · Per-client keying via JSONLogic · Platform & per-channel limits |
| [Backpressure](../features/scalability.md#backpressure) | Semaphore concurrency limits · 503 load shedding · Per-channel configuration |
| [Async Processing](../features/scalability.md#async-processing) | Multi-worker trace queue · Bounded buffer channels · DLQ retry processor |
| [Horizontal Scaling](../features/scalability.md#horizontal-scaling) | Stateless instances · Channel include/exclude filters · Multi-database backends |

## Deployability — C

| Area | Capabilities |
|------|-------------|
| [Packaging](../features/deployability.md#packaging) | Single binary · SQLite, PostgreSQL, MySQL · Minimal footprint |
| [Containerization](../features/deployability.md#containerization) | Multi-stage Docker build · Non-root execution · Built-in health probes |
| [Configuration](../features/deployability.md#configuration) | TOML + env var overrides · Sensible defaults · Runtime configuration |
| [Distribution](../features/deployability.md#distribution) | Homebrew tap · Shell & PowerShell installers · Multi-platform binaries |

## Extensibility — S

| Area | Capabilities |
|------|-------------|
| [Connectors](../features/extensibility.md#connectors) | HTTP & Webhooks · Kafka pub/sub · Database (SQL) · Cache (Memory & Redis) · Storage (S3/GCS) · MongoDB (NoSQL) |
| [Custom Functions](../features/extensibility.md#custom-functions) | Async function handlers · Built-in function library · JSONLogic expressions |
| [Channel Protocols](../features/extensibility.md#channel-protocols) | REST with route matching (sync) · Simple HTTP (sync) · Kafka (async) |

## Availability — C

| Area | Capabilities |
|------|-------------|
| [Hot-Reload](../features/availability.md#hot-reload) | Zero-downtime engine swap · Channel registry rebuild · Kafka consumer restart |
| [Canary Rollouts](../features/availability.md#canary-rollouts) | Percentage-based traffic split · Gradual migration · Instant rollback |
| [Versioning](../features/availability.md#versioning) | Draft / Active / Archived lifecycle · Multi-version history · Workflow import & export |
| [Performance](../features/availability.md#performance) | Response caching · Request deduplication · Connection pool caching |

## Maintainability — B

| Area | Capabilities |
|------|-------------|
| [Admin APIs](../features/maintainability.md#admin-apis) | Full CRUD for all entities · Version management · Engine control · OpenAPI / Swagger UI |
| [CI/CD Integration](../features/maintainability.md#cicd-integration) | Bulk import & export · Pre-deploy validation · GitOps-friendly |
| [Testing](../features/maintainability.md#testing) | Dry-run execution · Workflow validation · Step-by-step traces |
| [Operations](../features/maintainability.md#operations) | Audit logging · Database backup & restore · Config validation CLI |
