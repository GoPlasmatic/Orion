# Dev & Prod Environments

Orion's architecture reads differently depending on which way you approach it.
This page contrasts the two:

1. **Design time (dev)** — how channels, workflows, and connectors are *authored*,
   often by an LLM driving the Admin API through the CLI/MCP.
2. **Run time (prod)** — how production traffic is *served*, API- and event-driven,
   against a reliable, highly-available backend.

The split matters: **design time is human/AI-in-the-loop and write-heavy**, while
**run time is machine-driven and read/execute-heavy**. The same single binary
(`orion-server`) serves both planes — only the configuration and topology change.

> The building blocks referenced throughout this page — **Channel**, **Workflow**,
> and **Connector** — are introduced in
> [Architecture Overview → Three Primitives](../architecture/overview.md#three-primitives).
> Every object follows a `draft → active → archived` lifecycle and is independently
> versioned.

---

## Design-Time Architecture (Dev Environment)

At design time, an **LLM** (or a human operator) authors and validates services
through Orion's **Admin API**. The loop is: *generate → validate → dry-run →
activate → hot-reload* — no redeploy, no container rebuild.

```orion-diagram
{
  "direction": "TB",
  "groups": [
    { "id": "authors", "label": "🧑‍💻 Authors" },
    { "id": "clients", "label": "Design-Time Clients" },
    { "id": "orion", "label": "🟦 Orion (single binary — dev)" },
    { "id": "store", "label": "Embedded Storage (dev default)" }
  ],
  "nodes": [
    { "id": "LLM", "label": "🤖 LLM / AI Agent", "sublabel": "(Claude, etc.)", "type": "observability", "group": "authors", "shape": "actor" },
    { "id": "DEV", "label": "👤 Developer / Operator", "type": "observability", "group": "authors", "shape": "actor" },

    { "id": "CLI", "label": "Orion CLI", "sublabel": "orion-server lint / dry-run\nvalidate-config / migrate", "type": "service", "group": "clients" },
    { "id": "MCP", "label": "MCP Server", "sublabel": "tool-calls from LLM", "type": "service", "group": "clients" },
    { "id": "HTTP", "label": "HTTP client / Swagger UI", "sublabel": "GET /docs", "type": "service", "group": "clients" },
    { "id": "GIT", "label": "GitOps / CI-CD", "sublabel": "export → commit → import", "type": "service", "group": "clients" },

    { "id": "ADMIN", "label": "Admin API  /api/v1/admin/*", "sublabel": "• /workflows  (CRUD, versions, rollout)\n• /channels   (CRUD, versions, status)\n• /connectors (CRUD, circuit-breakers)\n• /engine     (status, reload)\n• /audit-logs · /backups · /functions", "type": "accent", "group": "orion" },
    { "id": "VALIDATE", "label": "Validation & Dry-Run", "sublabel": "POST /workflows/validate\nPOST /workflows/{id}/test\n(schema check + step traces + field-pathed errors)", "type": "accent", "group": "orion" },
    { "id": "LIFECYCLE", "label": "Lifecycle & Versioning", "sublabel": "draft → active → archived\n(single-draft + active-immutable\nenforced by DB triggers)", "type": "accent", "group": "orion" },
    { "id": "RELOAD", "label": "Engine Hot-Reload", "sublabel": "Arc<RwLock<Arc<Engine>>>\nzero-downtime swap on activate", "type": "accent", "group": "orion" },

    { "id": "SQLITE", "label": "SQLite", "sublabel": "orion.db — single file\nzero external dependencies\nembedded migrations", "type": "datastore", "group": "store" }
  ],
  "edges": [
    { "from": "ADMIN", "to": "VALIDATE" },
    { "from": "ADMIN", "to": "LIFECYCLE" },
    { "from": "LIFECYCLE", "to": "RELOAD", "label": "activate / archive / delete" },
    { "from": "LLM", "to": "CLI" },
    { "from": "LLM", "to": "MCP" },
    { "from": "DEV", "to": "CLI" },
    { "from": "DEV", "to": "HTTP" },
    { "from": "DEV", "to": "GIT" },
    { "from": "CLI", "to": "ADMIN" },
    { "from": "MCP", "to": "ADMIN" },
    { "from": "HTTP", "to": "ADMIN" },
    { "from": "GIT", "to": "ADMIN" },
    { "from": "LIFECYCLE", "to": "SQLITE" },
    { "from": "VALIDATE", "to": "SQLITE", "label": "read schemas", "style": "dashed" },
    { "from": "RELOAD", "to": "SQLITE", "label": "rebuild from DB", "style": "dashed" }
  ]
}
```

### What makes the dev loop fast

- **AI-authored, governed by the platform.** *"AI generates workflows, Orion
  provides the governance."* Whatever the LLM produces, the platform enforces
  health checks, metrics, retries, and validation.
- **Dry-run before you ship.** `POST /workflows/{id}/test` runs the pipeline on a
  sample payload and returns per-task timing and field-pathed validation errors
  (e.g. `tasks[0].function.input.connector`).
- **Drafts are free.** Creating/updating drafts never touches the live engine —
  only `activate` / `archive` / `delete` / `rollout` trigger an audit-logged
  **hot-reload** (the inner `Arc<Engine>` is swapped while readers keep the old).
- **GitOps-friendly.** Workflows and channels are plain JSON: `export` → commit →
  PR review → `validate` → `import as drafts` → `test` → `activate`.
- **Zero-dependency local.** The dev box runs the **single binary against embedded
  SQLite** — `./orion-server` and you have the full Admin API + Swagger UI at
  `/docs`.

See also: [Maintainability](../features/maintainability.md) (Admin APIs, CI/CD,
dry-run) and the [Admin API reference](../api/admin.md).

---

## Run-Time Architecture (Prod Environment)

In production the picture inverts: traffic is **API- and event-driven**, served by
**stateless Orion instances** behind a load balancer, backed by a **reliable,
highly-available database** plus optional Redis / MongoDB / Kafka / OTel.

```orion-diagram
{
  "direction": "TB",
  "groups": [
    { "id": "ingress", "label": "Ingress" },
    { "id": "inst", "label": "🟦 Orion Fleet · stateless instance (orion-server)" },
    { "id": "data", "label": "Reliable Backends" },
    { "id": "obs", "label": "Observability" }
  ],
  "nodes": [
    { "id": "REST", "label": "REST / HTTP clients", "sublabel": "POST /api/v1/data/{channel}\nGET  /api/v1/data/orders/{id}", "type": "service", "group": "ingress" },
    { "id": "ASYNC", "label": "Async clients", "sublabel": "POST .../{channel}/async\n→ trace_id, poll later", "type": "service", "group": "ingress" },
    { "id": "PRODUCER", "label": "Kafka producers", "sublabel": "(event streams)", "type": "service", "group": "ingress" },

    { "id": "LB", "label": "⚖️ Load Balancer / API Gateway", "sublabel": "TLS, health probes /readyz /healthz", "type": "gateway", "shape": "hexagon" },

    { "id": "MW", "label": "Middleware stack", "sublabel": "panic-recovery · OTel · metrics · admin-auth\n· rate-limit · body-limit · compression\n· security-headers · request-id · CORS", "type": "accent", "group": "inst" },
    { "id": "REGISTRY", "label": "Channel Registry  (in-memory)", "sublabel": "route table · dedup · rate limiter\n· input validation (JSONLogic)\n· backpressure semaphore · response cache", "type": "accent", "group": "inst" },
    { "id": "ENGINE", "label": "Dataflow Engine", "sublabel": "Arc<RwLock<Arc<Engine>>>\nworkflow matcher (JSONLogic + rollout %)\n→ ordered task pipeline", "type": "accent", "group": "inst" },
    { "id": "FUNCS", "label": "Custom Functions", "sublabel": "http_call · channel_call\ndb_read/db_write · cache_read/write\nmongo_read · publish_kafka", "type": "accent", "group": "inst" },
    { "id": "KAFKAC", "label": "Kafka Consumer", "sublabel": "(topic → channel)", "type": "accent", "group": "inst" },
    { "id": "QUEUE", "label": "Async Trace Queue + DLQ", "sublabel": "(bounded, multi-worker, auto-retry)", "type": "accent", "group": "inst", "shape": "queue" },

    { "id": "PG", "label": "PostgreSQL / MySQL", "sublabel": "primary + replica (HA)\nworkflows · channels · connectors\ntraces · trace_dlq · audit_logs", "type": "datastore", "group": "data" },
    { "id": "REDIS", "label": "Redis", "sublabel": "cache · dedup · response cache", "type": "datastore", "group": "data" },
    { "id": "MONGO", "label": "MongoDB", "sublabel": "document reads", "type": "datastore", "group": "data" },
    { "id": "KAFKA", "label": "Kafka cluster", "sublabel": "ingest · egress · DLQ", "type": "datastore", "group": "data", "shape": "queue" },
    { "id": "EXT", "label": "External HTTP services", "sublabel": "(Stripe, internal APIs, webhooks)\n+ circuit breakers + retries", "type": "datastore", "group": "data", "shape": "cloud" },

    { "id": "OTEL", "label": "OpenTelemetry Collector", "sublabel": "OTLP / W3C trace context", "type": "observability", "group": "obs" },
    { "id": "PROM", "label": "Prometheus → Grafana", "sublabel": "scrape /metrics", "type": "observability", "group": "obs" }
  ],
  "edges": [
    { "from": "REST", "to": "LB" },
    { "from": "ASYNC", "to": "LB" },
    { "from": "PRODUCER", "to": "KAFKA" },
    { "from": "LB", "to": "MW" },
    { "from": "MW", "to": "REGISTRY" },
    { "from": "REGISTRY", "to": "ENGINE" },
    { "from": "ENGINE", "to": "FUNCS" },
    { "from": "KAFKAC", "to": "ENGINE" },
    { "from": "ENGINE", "to": "QUEUE" },
    { "from": "KAFKA", "to": "KAFKAC" },
    { "from": "FUNCS", "to": "PG", "label": "db_read / db_write" },
    { "from": "FUNCS", "to": "REDIS", "label": "cache_read / write" },
    { "from": "FUNCS", "to": "MONGO", "label": "mongo_read" },
    { "from": "FUNCS", "to": "KAFKA", "label": "publish_kafka" },
    { "from": "FUNCS", "to": "EXT", "label": "http_call" },
    { "from": "REGISTRY", "to": "REDIS", "label": "dedup / cache", "style": "dashed" },
    { "from": "ENGINE", "to": "PG", "label": "load config · persist traces" },
    { "from": "QUEUE", "to": "PG", "label": "store traces / DLQ" },
    { "from": "ENGINE", "to": "OTEL", "label": "spans", "style": "dashed" },
    { "from": "ENGINE", "to": "PROM", "label": "scrape", "style": "dashed" }
  ]
}
```

The data-plane request pipeline (route resolution → channel registry → engine →
task pipeline → response) is detailed in
[Architecture Overview → Request Processing Flow](../architecture/overview.md#request-processing-flow).

### What makes prod reliable

- **Stateless, horizontally scalable.** Instances hold no session state; scale out
  behind a load balancer. Use `include`/`exclude` channel filters to shard
  workloads across instances.
- **Reliable DB with HA.** Point `storage.url` at **PostgreSQL** (or MySQL) with a
  primary/replica setup. The *same* binary that ran on embedded SQLite in dev now
  speaks Postgres — backend is auto-detected from the URL scheme; migrations are
  embedded per backend.
- **Resilience built in.** Per-connector **circuit breakers** (auto-recovery),
  **retries** with exponential backoff (capped at 60s), per-channel / per-query /
  per-request **timeouts**, **backpressure** semaphores (503 load shedding), and a
  **Dead Letter Queue** with automatic retry for failed async traces.
- **Zero-downtime changes.** Hot-reload swaps the engine without dropping
  in-flight requests; canary **rollouts** split traffic by percentage with instant
  rollback.
- **In-process composition.** `channel_call` invokes another channel's workflow
  **in-process** — no network round-trip, with cycle/depth detection.
- **Full observability.** Structured JSON logs, Prometheus metrics at `/metrics`,
  OpenTelemetry spans over OTLP with W3C trace-context propagated even through
  Kafka headers, and `/health` · `/healthz` · `/readyz` for orchestrators.
- **Security.** Secret masking, SSRF protection (private-IP blocking), TLS,
  security headers, admin-API auth, parameterized SQL, JSONLogic input validation.

See also: [Resilience](../features/resilience.md),
[Scalability](../features/scalability.md),
[Availability](../features/availability.md), and
[Observability](../features/observability.md).

---

## Dev → Prod: One Binary, Two Topologies

The key design decision is that **nothing about the artifacts changes between
environments** — only the configuration and the backends they point at.

```orion-diagram
{
  "direction": "LR",
  "groups": [
    { "id": "dev", "label": "DEV — design time" },
    { "id": "prod", "label": "PROD — run time" }
  ],
  "nodes": [
    { "id": "D1", "label": "LLM / Operator", "type": "observability", "group": "dev", "shape": "actor" },
    { "id": "D2", "label": "Admin API", "sublabel": "(CLI · MCP · Swagger)", "type": "accent", "group": "dev" },
    { "id": "D3", "label": "SQLite", "sublabel": "single file", "type": "datastore", "group": "dev" },

    { "id": "ARTIFACTS", "label": "📦 Versioned JSON artifacts", "sublabel": "channels · workflows · connectors\n(export → git → import)", "type": "infra" },

    { "id": "P1", "label": "API · Kafka traffic", "type": "service", "group": "prod" },
    { "id": "P2", "label": "Orion fleet", "sublabel": "(N stateless instances)", "type": "service", "group": "prod" },
    { "id": "P3", "label": "PostgreSQL / MySQL", "sublabel": "+ Redis · Mongo · Kafka · OTel", "type": "datastore", "group": "prod" }
  ],
  "edges": [
    { "from": "D1", "to": "D2" },
    { "from": "D2", "to": "D3" },
    { "from": "P1", "to": "P2" },
    { "from": "P2", "to": "P3" },
    { "from": "D3", "to": "ARTIFACTS", "label": "export / import · GitOps · CI-CD" },
    { "from": "ARTIFACTS", "to": "P1", "label": "validate → import drafts → activate" }
  ]
}
```

| Dimension | Dev (design time) | Prod (run time) |
|-----------|-------------------|-----------------|
| **Primary driver** | LLM / operator via Admin API | API & Kafka traffic on the data plane |
| **Dominant traffic** | Write-heavy (author, validate, activate) | Read/execute-heavy (serve requests) |
| **Entry points** | CLI · MCP · Swagger UI · GitOps | REST · async · Kafka behind a load balancer |
| **Database** | Embedded **SQLite** (`orion.db`, zero deps) | **PostgreSQL / MySQL** with HA (primary + replica) |
| **Optional backends** | usually none | Redis, MongoDB, Kafka, OTel collector, Prometheus |
| **Scaling** | single local instance | N stateless instances, horizontally scaled |
| **Config** | minimal / defaults, `./orion-server` | TLS, admin-auth, rate limits, tracing, pools tuned |
| **Engine reload** | constant (every activate) | controlled, audit-logged, zero-downtime |

---

## Deployment Notes

The same `orion-server` binary ships everywhere — all three DB backends, Kafka
producer/consumer, OTLP export, TLS, and Swagger UI are compiled in (no feature
flags, no plugins). Configuration is a TOML file (`-c config.toml`) with every key
overridable via `ORION_SECTION__KEY` environment variables
(e.g. `ORION_STORAGE__URL=postgres://…`).

For packaging, containerization, multi-instance scaling, and the full set of config
keys, see [Deployability](../features/deployability.md) and the
[Configuration Reference](../configuration/reference.md).
