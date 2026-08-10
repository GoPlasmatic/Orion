# Dev & Prod Environments

Orion's architecture reads differently depending on which way you approach it.
This page contrasts the two:

1. **Design time (dev)** — how channels, workflows, and connectors are *authored*,
   often by an LLM driving the Admin API through the CLI/MCP.
2. **Run time (prod)** — how production traffic is *served*, API- and event-driven,
   by a cluster of replicas that behave as a single logical system.

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
    { "id": "RELOAD", "label": "Engine Hot-Reload", "sublabel": "ArcSwap publish — wait-free readers\nzero-downtime swap on activate", "type": "accent", "group": "orion" },

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
  **hot-reload** (the new engine is published atomically; in-flight requests
  keep the one they started with).
- **GitOps-friendly.** Workflows and channels are plain JSON: `export` → commit →
  PR review → `validate` → `import as drafts` → `test` → `activate`. The entities
  of one service ship together as a [**package**](./packages.md) — one versioned
  artifact, promoted between instances with a receipt on the target.
- **Zero-dependency local.** The dev box runs the **single binary against embedded
  SQLite** — `./orion-server` and you have the full Admin API + Swagger UI at
  `/docs`.

See also: [Maintainability](../features/maintainability.md) (Admin APIs, CI/CD,
dry-run) and the [Admin API reference](../api/admin.md).

---

## Run-Time Architecture (Prod Environment)

In production the picture inverts: traffic is **API- and event-driven**, served by
**N identical replicas in cluster mode** behind a load balancer. The replicas
share one **PostgreSQL/MySQL** database and one **Redis**, and behave as a single
logical system — a config change made through any node reaches all nodes, and
dedup, rate limits, and response caches hold across the fleet.

```orion-diagram
{
  "direction": "TB",
  "groups": [
    { "id": "ingress", "label": "Ingress" },
    { "id": "inst", "label": "🟦 Orion Fleet · N replicas in cluster mode (orion-server)" },
    { "id": "data", "label": "Shared Backends" },
    { "id": "obs", "label": "Observability" }
  ],
  "nodes": [
    { "id": "REST", "label": "REST / HTTP clients", "sublabel": "POST /api/v1/data/{channel}\nGET  /api/v1/data/orders/{id}", "type": "service", "group": "ingress" },
    { "id": "ASYNC", "label": "Async clients", "sublabel": "POST .../{channel}/async\n→ trace_id, poll later", "type": "service", "group": "ingress" },
    { "id": "PRODUCER", "label": "Kafka producers", "sublabel": "(event streams)", "type": "service", "group": "ingress" },

    { "id": "LB", "label": "⚖️ Load Balancer / API Gateway", "sublabel": "TLS, health probes /readyz /healthz\ngraceful drain on SIGTERM", "type": "gateway", "shape": "hexagon" },

    { "id": "MW", "label": "Middleware stack", "sublabel": "panic-recovery · OTel · metrics · admin-auth\n· rate-limit · body-limit · compression\n· security-headers · request-id · CORS", "type": "accent", "group": "inst" },
    { "id": "REGISTRY", "label": "Channel Registry  (in-memory)", "sublabel": "route table · dedup · rate limiter\n· input validation (JSONLogic)\n· backpressure semaphore · response cache", "type": "accent", "group": "inst" },
    { "id": "ENGINE", "label": "Dataflow Engine", "sublabel": "ArcSwap — wait-free engine load\nworkflow matcher (JSONLogic + rollout %)\n→ ordered task pipeline", "type": "accent", "group": "inst" },
    { "id": "FUNCS", "label": "Custom Functions", "sublabel": "http_call · channel_call\ndb_read/db_write · cache_read/write\nmongo_read · publish_kafka", "type": "accent", "group": "inst" },
    { "id": "EPOCH", "label": "Cluster Coordination", "sublabel": "epoch watcher — polls config_epoch,\nresyncs engine + connectors on change\njob leases: cleanup / DLQ retry single-flight", "type": "accent", "group": "inst" },
    { "id": "KAFKAC", "label": "Kafka Consumer", "sublabel": "(topic → channel)\nstatic group membership per instance_id", "type": "accent", "group": "inst" },
    { "id": "QUEUE", "label": "Async Trace Queue + DLQ", "sublabel": "(bounded, multi-worker, auto-retry)", "type": "accent", "group": "inst", "shape": "queue" },

    { "id": "PG", "label": "PostgreSQL / MySQL", "sublabel": "primary + replica (HA)\nworkflows · channels · connectors\ntraces · trace_dlq · audit_logs\nconfig_epoch (cluster coordination)", "type": "datastore", "group": "data" },
    { "id": "REDIS", "label": "Redis (shared)", "sublabel": "cluster coordination:\ndedup · response cache\nper-channel rate limits", "type": "datastore", "group": "data" },
    { "id": "MONGO", "label": "MongoDB", "sublabel": "document reads", "type": "datastore", "group": "data" },
    { "id": "KAFKA", "label": "Kafka cluster", "sublabel": "ingest · egress · DLQ", "type": "datastore", "group": "data", "shape": "queue" },
    { "id": "EXT", "label": "External HTTP services", "sublabel": "(Stripe, internal APIs, webhooks)\n+ circuit breakers + retries", "type": "datastore", "group": "data", "shape": "cloud" },

    { "id": "OTEL", "label": "OpenTelemetry Collector", "sublabel": "OTLP / W3C trace context", "type": "observability", "group": "obs" },
    { "id": "PROM", "label": "Prometheus → Grafana", "sublabel": "scrape /metrics on every replica", "type": "observability", "group": "obs" }
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
    { "from": "REGISTRY", "to": "REDIS", "label": "shared dedup · rate-limit · cache", "style": "dashed" },
    { "from": "EPOCH", "to": "PG", "label": "poll config_epoch", "style": "dashed" },
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

### Cluster mode: N replicas as one logical system

Production scaling is **cluster mode** — enable it and run identical replicas
behind the load balancer:

```toml
[cluster]
enabled = true
redis_url = "redis://redis:6379"   # required; shared dedup / response cache / rate limits
epoch_poll_interval_ms = 2000      # how often nodes poll for config changes
instance_id = ""                   # auto-generated UUID when empty (max 64 chars —
                                   # doubles as the Kafka group.instance.id)

[storage]
url = "postgres://orion:orion@postgres:5432/orion"
auto_migrate = false               # run `orion-server migrate` as a deploy step
```

Cluster mode **requires** PostgreSQL or MySQL plus the shared Redis — startup
refuses `sqlite:`, which is single-host by construction. What the two shared
backends coordinate:

- **Config changes reach every node.** Every admin mutation (activate, archive,
  rollout, connector change, manual reload) advances a shared **config epoch** in
  the database; each replica polls it every `epoch_poll_interval_ms` (default
  2 s) and resyncs its engine, connector registry, and cached pools. A change
  made through *any* node reaches *all* nodes — no fan-out scripting, no rolling
  restart. Circuit-breaker resets fan out over the same epoch bus.
- **Shared dedup, rate limits, and response caches.** On the cluster Redis, the
  same idempotency key on two nodes gets exactly one execution (and a `409` for
  the replay), per-channel rate limits hold across **all replicas combined**, and
  cache hits are shared instead of each node warming its own. A channel whose
  dedup/cache connector is missing, broken, or explicitly in-memory **refuses to
  load** rather than silently degrading to per-node state.
- **Background jobs single-flight.** Trace cleanup and DLQ retry acquire a
  per-tick job lease, so only one node runs each job; Kafka consumers use static
  group membership (`group.instance.id` = the instance id), so rolling restarts
  rejoin without a full rebalance.

Some components intentionally stay **per-node** with documented ×N semantics:
circuit breakers trip independently, `backpressure.max_concurrent_per_node` and
platform-level `[rate_limit]` are per node, and `/metrics` is scraped per
replica. Filesystem backups (`POST /api/v1/admin/backups`) return `400` in
cluster mode — use your database's native snapshot/PITR tooling instead. See
[Scalability → Horizontal Scaling](../features/scalability.md#horizontal-scaling--cluster-mode)
for the full coordination tables.

### Migrations are a deploy step, not a boot race

In cluster mode, replicas never migrate at boot: set
`storage.auto_migrate = false` and run **`orion-server migrate`** before new
replicas start (a replica that boots against a pending migration fails fast). A
production cluster left on `auto_migrate = true` is **refused at startup** rather
than allowed to race. Both packaged deployments wire this in: the Helm chart runs
migrations as a `pre-install`/`pre-upgrade` Job, and `docker-compose.ha.yml` has
a one-shot `migrate` service that completes before either node boots.

### Two packaged ways to run it

- **[Helm chart](./kubernetes.md)** (`deploy/helm/orion`) — deploys the cluster shape on
  Kubernetes: 2 replicas by default (optional CPU-based HPA), the pre-upgrade
  migration Job, graceful rolling deploys that surge instead of dipping
  (`maxUnavailable: 0`, `maxSurge: 1`, `/readyz` drain), a PodDisruptionBudget,
  anti-affinity across nodes, hardened pod security defaults, and a dedicated
  unauthenticated metrics listener for Prometheus. It installs as
  `ORION_ENVIRONMENT=production`, so admin API keys are required up front.
  Point it at your managed database and Redis
  (`storage.url`, `cluster.redisUrl`).
- **`docker-compose.ha.yml`** (repo root) — the reference topology for a single
  host or a smoke test of the production shape: nginx load balancer → 2× Orion
  in cluster mode → shared Postgres + Redis, plus the one-shot `migrate`
  service. `deploy/ha/rolling-drill.sh` demonstrates a zero-5xx rolling deploy
  against it.

### What makes prod reliable

- **Horizontally scalable as one system.** Replicas hold no session state, and
  cluster mode moves the cross-request state (dedup windows, rate-limit windows,
  response caches) to the shared Redis — so adding a replica is invisible to
  callers. For dedicated capacity, `include`/`exclude` channel filters can still
  shard channel groups across instance pools sharing one database — a topology
  refinement layered on top, no longer the scaling mechanism itself.
- **Reliable DB with HA.** Point `storage.url` at **PostgreSQL** (or MySQL) with a
  primary/replica setup. The *same* binary that ran on embedded SQLite in dev now
  speaks Postgres — backend is auto-detected from the URL scheme; migrations are
  embedded per backend.
- **Resilience built in.** Per-connector **circuit breakers** (auto-recovery),
  **retries** with exponential backoff (capped at 60s), per-channel / per-query /
  per-request **timeouts**, **backpressure** semaphores (503 load shedding), and a
  **Dead Letter Queue** with automatic retry for failed async traces.
- **Zero-downtime changes.** Hot-reload swaps the engine without dropping
  in-flight requests — cluster-wide via the config epoch; canary **rollouts**
  split traffic by percentage with sticky assignment that holds across replicas,
  and instant rollback. On `SIGTERM`, `/readyz` flips to 503 immediately and the
  node keeps serving through the drain window, so rolling deploys are zero-5xx.
- **In-process composition.** `channel_call` invokes another channel's workflow
  **in-process** — no network round-trip, with cycle/depth detection.
- **Full observability.** Structured JSON logs, Prometheus metrics at `/metrics`
  (per replica), OpenTelemetry spans over OTLP with W3C trace-context propagated
  even through Kafka headers, and `/health` · `/healthz` · `/readyz` for
  orchestrators.
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

    { "id": "ARTIFACTS", "label": "📦 Package artifacts (versioned JSON)", "sublabel": "channels · workflows · connectors\n(export → git → apply)", "type": "infra" },

    { "id": "P1", "label": "API · Kafka traffic", "type": "service", "group": "prod" },
    { "id": "P2", "label": "Orion fleet", "sublabel": "(N replicas · cluster mode)", "type": "service", "group": "prod" },
    { "id": "P3", "label": "PostgreSQL / MySQL + Redis", "sublabel": "shared config + coordination\n+ Mongo · Kafka · OTel", "type": "datastore", "group": "prod" }
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
| **Database** | Embedded **SQLite** (`orion.db`, zero deps) | **PostgreSQL / MySQL** with HA (SQLite refused in cluster mode) |
| **Coordination** | none — single process | **cluster mode:** config epoch in DB + shared Redis (dedup, rate limits, caches) |
| **Migrations** | at boot (`auto_migrate`, the default) | `orion-server migrate` as a deploy step; replicas never migrate at boot |
| **Other backends** | usually none | MongoDB, Kafka, OTel collector, Prometheus |
| **Scaling** | single local instance | N identical replicas behind an LB, one logical system |
| **Config** | minimal / defaults, `./orion-server` | `environment = "production"`: admin-auth enforced, TLS, rate limits, pools tuned |
| **Engine reload** | constant (every activate) | controlled, audit-logged, zero-downtime, cluster-wide via the epoch |

---

## Deployment Notes

The same `orion-server` binary ships everywhere — all three DB backends, cluster
mode, Kafka producer/consumer, OTLP export, TLS, and Swagger UI are compiled in
(no feature flags, no plugins). Configuration is a TOML file (`-c config.toml`)
with every key overridable via `ORION_SECTION__KEY` environment variables
(e.g. `ORION_STORAGE__URL=postgres://…`, `ORION_CLUSTER__ENABLED=true`).

Start from the packaged topologies — the Helm chart (`deploy/helm/orion`, see its
README) on Kubernetes, or `docker-compose.ha.yml` elsewhere — and adjust. For
packaging, containerization, and the full set of config keys, see
[Deployability](../features/deployability.md) and the
[Configuration Reference](../configuration/reference.md).
