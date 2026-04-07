# Orion: Declarative Services Runtime

## Vision

Orion evolves from a JSONLogic rules engine into a **declarative services runtime** — doing for services what databases did for data. Developers define service logic through admin APIs, not code. Orion handles the runtime, connectivity, and orchestration.

A developer creates a microservice by defining a **channel** (the endpoint), wiring it to a **workflow** (the logic), and connecting it to **connectors** (the external world). No compilation, no deployment pipelines, no boilerplate — just configuration.

Orion has two modes of operation:
- **Design-time** — admin APIs to define channels, build workflows, configure connectors, test with dry-run, and manage versions
- **Runtime** — the execution engine that processes live traffic through configured workflows

## Core Concepts

### Channels

A channel is the unit of deployment. It encapsulates a complete microservice boundary and operates in one of two modes: **sync** or **async**.

#### Sync Channels

Sync channels expose request-response interfaces over HTTP. Two protocol flavors:

| Protocol | Description | Example |
|---|---|---|
| **REST** | Full RESTful endpoints with HTTP methods, path params, query params | `GET /orders/{id}`, `POST /orders` |
| **Simple HTTP** | Raw HTTP request/response — no REST conventions, no schema | `POST /webhook/stripe` — accepts any payload, runs a workflow |

Each sync channel definition includes: protocol type, route pattern, HTTP method(s), and the workflow to execute. The response is returned directly to the caller.

#### Async Channels

Async channels are event-driven — they consume from a message source, execute a workflow, and optionally produce to a destination. No HTTP request/response cycle.

| Transport | Description | Example |
|---|---|---|
| **Kafka** | Consume from topic, process, optionally produce to another topic | Topic `order.placed` → workflow → topic `order.confirmed` |

Async channels define: transport type, source (topic/queue), consumer group, and the workflow to execute. Results can be published to connectors as a workflow step. The internal transport abstraction is designed for extensibility — additional transports (AMQP, NATS, SQS) can be added later as new connector types without engine changes.

#### Sync-to-Async Bridging

No special mechanism is needed. A sync workflow can include a `publish_kafka` step and return 202 Accepted. An async channel picks it up from there. Bridging is a usage pattern, not a platform feature.

#### Channel Examples

```
# Sync — REST
POST /api/v1/data/orders              → order creation workflow
GET  /api/v1/data/orders/{id}         → order lookup workflow

# Sync — Simple HTTP
POST /api/v1/data/webhook/stripe      → payment webhook workflow

# Async — Kafka
Topic: order.placed                   → order processing workflow
Topic: user.signup                    → welcome email workflow
```

#### Schemas

Schemas are **optional, never mandatory**. Channels accept any payload by default — string, blob, or structured JSON. A parser step in the workflow can discover the structure and route accordingly, enabling gateway-like patterns. Developers can attach JSON Schema to a channel when they want input validation or documentation generation.

### Platform Baseline — Built-in Service Abstractions

Every channel gets production-grade service abstractions out of the box. Developers configure them per channel — the platform handles enforcement. This is what makes Orion a runtime platform, not just a workflow runner.

#### Per-Channel Configuration

These are set once per channel and the platform handles them automatically at the channel boundary, before and after the workflow executes:

| Concern | What it does | Configuration |
|---|---|---|
| **Rate limiting** | Throttle requests to protect the service | Requests per second/minute, per-client or global, burst allowance |
| **Timeout** | Enforce a maximum execution time for the entire workflow | Duration in milliseconds — request is cancelled and returns 504 if exceeded |
| **Caching** | Cache responses for identical requests | TTL, cache key definition (headers, path params, body fields), invalidation rules |
| **Input validation** | Reject bad requests at the boundary | JSONLogic `validation_logic` with access to headers (content-type, content-length), query params, and path params |
| **CORS** | Control browser cross-origin access | Allowed origins, methods, headers per channel |
| **Backpressure** | Shed load when overwhelmed | Max concurrent requests, queue depth — returns 503 when exceeded |
| **Request deduplication** | Prevent duplicate processing | Idempotency key header name, dedup window duration |
| **Response compression** | Reduce payload size over the wire | Enable/disable, minimum size threshold, algorithms (gzip, brotli) |
| **Versioning** | Route by API version | Path-based: create channels with versioned route patterns (e.g., `/v1/orders/{id}`, `/v2/orders/{id}`) pointing to different workflows |

Example channel configuration with baseline features:
```json
{
  "name": "orders.create",
  "type": "sync",
  "protocol": "rest",
  "method": "POST",
  "path": "/orders",
  "workflow": "order-creation-workflow",
  "config": {
    "rate_limit": { "requests_per_second": 100, "burst": 20, "per_client": true },
    "timeout_ms": 5000,
    "cache": { "enabled": false },
    "validation_logic": { "and": [{ "<=": [{ "var": "metadata.headers.content-length" }, 1048576] }] },
    "cors": { "allowed_origins": ["https://admin.example.com"] },
    "backpressure": { "max_concurrent": 200 },
    "deduplication": { "header": "Idempotency-Key", "window_secs": 300 },
    "compression": { "enabled": true, "min_bytes": 1024 }
  }
}
```

All fields have sensible defaults. A minimal channel definition needs only name, type, and workflow — everything else falls back to platform defaults.

#### Platform-Wide Automatic (Zero Config)

These are always on for every channel. Developers never configure them — the platform provides them as a guarantee:

| Concern | What it does |
|---|---|
| **Structured logging** | Every request logged with channel, request ID, duration, status. JSON format in production, pretty in development. |
| **Metrics** | Prometheus metrics per channel: request count, latency histogram, error rate, active requests |
| **Distributed tracing** | Trace context propagation (W3C), per-step span tracking, trace ID in responses |
| **Error handling** | Consistent JSON error format across all channels: `{"error": {"code": "...", "message": "..."}}` |
| **Request ID** | UUID generated per request, propagated through workflow, returned in `x-request-id` header |
| **Health reporting** | Channel health reflected in `/health` endpoint — degraded if circuit breakers trip or backpressure kicks in |

The developer's mental model: **define the workflow logic, configure the channel policies, and the platform handles everything else.**

### Workflows

Each channel is backed by a workflow — a pipeline of steps that define what the service does. Workflows replace hand-written service code. The workflow engine is powered by `dataflow-rs` and is the same regardless of whether the channel is sync or async.

A workflow is a directed sequence of tasks with branching:

```
Request → Parse → Validate → Enrich (HTTP) → Transform → Store (DB) → Publish (Kafka) → Respond
```

**Branching** is supported natively via `dataflow-rs`. Each step can have a JSONLogic condition that determines which path the workflow takes.

**No loops.** Iteration is modeled via `map` over arrays within a step or by chaining async channels (each Kafka message triggers a new stateless workflow execution).

**Stateless execution.** Each request or message is processed independently — no saga orchestration or workflow resumption in the engine. Developers who need state use `DBConnector` or `CacheConnector` within workflow steps. Saga-like patterns are composed by chaining async channels, where each step writes progress to a DB and publishes the next event.

Each step in the workflow:
- Executes a **function** (built-in or via connector)
- Has an optional **condition** (JSONLogic) for branching
- Can **transform** data between steps using JSONLogic expressions
- Supports **error handling** (retry, fallback, continue-on-error)

#### Built-in Workflow Functions

| Function | Description |
|---|---|
| `parse_json` | Parse JSON payloads into workflow data context |
| `parse_xml` | Parse XML payloads into structured JSON |
| `map` | Transform/reshape data using JSONLogic expressions |
| `filter` | Allow/halt processing based on conditions |
| `validate` | Enforce required fields and schema-like constraints |
| `http_call` | Invoke external APIs via HTTPConnector (with retry + circuit breaker) |
| `db_read` | Execute SELECT queries via DBConnector, returns rows as JSON |
| `db_write` | Execute INSERT/UPDATE/DELETE via DBConnector, returns affected count |
| `cache_read` | Read from Redis cache via CacheConnector, returns parsed JSON |
| `cache_write` | Write to Redis cache via CacheConnector, with optional TTL |
| `mongo_read` | Execute MongoDB find queries via MongoDBConnector, BSON-to-JSON conversion |
| `publish_json` | Serialize data to JSON output |
| `publish_xml` | Serialize data to XML output |
| `publish_kafka` | Publish messages to Kafka topics via KafkaConnector |
| `channel_call` | Invoke another channel's workflow in-process (same interface as `http_call`) |
| `log` | Emit structured log entries for auditing |

### Connectors

Connectors are pre-configured, reusable connections to external systems. Workflows reference connectors by name — never by credentials or connection strings.

| Connector Type | Capabilities |
|---|---|
| **HTTPConnector** | REST API calls, webhooks, external service integration |
| **DBConnector** | Parameterized SQL queries (`db_read`, `db_write`) against Postgres, MySQL, SQLite. Connection pooling and timeouts managed by the connector. |
| **KafkaConnector** | Produce to topics, consume configuration for async channels |
| **CacheConnector** | Redis cache for lookups and session state (`cache_read`, `cache_write`) |
| **StorageConnector** | S3/GCS/local file read/write |
| **MongoDBConnector** | MongoDB document queries (`mongo_read`) with BSON-to-JSON conversion |

Connectors carry their own configuration: auth, retries, circuit breakers, timeouts, connection pooling. Authentication schemes supported: bearer token, basic auth, and API key.

**Secret masking** — sensitive fields (tokens, passwords, keys) are automatically masked in all API responses. Workflows never see raw credentials; they reference connectors by name.

## Design-Time (Admin APIs)

The developer experience — define, test, and deploy services through APIs:

- **Define channels** — create sync channels (REST or Simple HTTP with method, route pattern) or async channels (Kafka with topic, consumer group)
- **Build workflows** — compose task pipelines with branching, data transformation, and error handling
- **Configure connectors** — register external systems with credentials, retry policies, and circuit breaker settings
- **Test with dry-run** — execute workflows against sample data, inspect step-by-step execution traces
- **Version and rollout** — version history for every change, gradual traffic shifting with rollout percentages, instant rollback
- **Import/export** — bulk operations for CI/CD pipelines and GitOps workflows
- **Validate** — schema, task, connector reference, and JSONLogic syntax validation before deployment

### Authentication & Authorization

No built-in auth middleware. HTTP headers flow into the workflow context as data. Developers handle auth logic in workflow steps — a `validate` step that checks a JWT claim, an `http_call` to an external auth service, or a `filter` that rejects unauthorized requests. Higher-level auth features (JWT validation middleware, API key enforcement) will be layered on in future phases.

## Runtime (Data APIs)

The execution engine — processes live traffic through configured workflows:

- **Request routing** — sync: HTTP requests matched to channels by route pattern; async: Kafka messages consumed from topics and routed to channels
- **Workflow execution** — step-by-step pipeline processing with data context propagation (same engine for both sync and async)
- **Connector invocation** — external calls with retry, circuit breaking, and timeout
- **Inter-channel calls** — `channel_call` routes in-process when the target channel is local on the same instance
- **Observability** — traces, Prometheus metrics, and structured logs for every request without extra configuration
- **Scaling** — horizontal scaling with stateless instances

## Deployment

### Topology

The developer decides the deployment topology. The same channel definition works in all topologies — only the deployment configuration changes. Sync and async channels can coexist in any topology.

| Topology | Description |
|---|---|
| **Microservice** | Single channel (or small set) per Orion instance, independently scaled |
| **Modular Monolith** | Related channels grouped in one instance with clear boundaries |
| **Monolith** | All channels in a single instance |

### Channel Loading

The database is the source of truth for all channels and workflows. By default, an Orion instance loads all channels configured in the DB. The config file supports **include** and **exclude** lists to filter which channels an instance loads — this is how deployment topology is controlled.

```toml
# Load only order-related channels
[channels]
include = ["orders.*", "payments.*"]

# Or load everything except analytics
[channels]
exclude = ["analytics.*"]
```

Isolation between channel groups is a deployment concern, not a runtime enforcement. If a `channel_call` targets a channel not loaded on the current instance, it fails fast with a clear error.

### Deployment Options

- **Standalone** — direct binary execution
- **Docker** — containerized deployment (multi-stage build)
- **Kubernetes** — no special requirements, stateless instances
- **Sidecar** — co-located with an application

## Example: Order Processing Microservice

**Step 1 — Register connectors:**
```
POST /admin/connectors  →  "payments-api" (HTTPConnector to Stripe)
POST /admin/connectors  →  "orders-db" (DBConnector to Postgres)
POST /admin/connectors  →  "events" (KafkaConnector to event bus)
```

**Step 2 — Define the channel:**
```
POST /admin/channels  →  "orders.create" (POST /orders, sync REST)
```

**Step 3 — Build the workflow:**
```json
{
  "channel": "orders.create",
  "steps": [
    { "function": "validate", "config": { "required": ["item_id", "quantity", "customer_id"] } },
    { "function": "db_read", "connector": "orders-db", "config": { "query": "SELECT credit FROM customers WHERE id = $1", "params": ["data.customer_id"] } },
    { "function": "filter", "condition": { ">=": [{ "var": "data.credit" }, { "var": "data.total" }] } },
    { "function": "http_call", "connector": "payments-api", "config": { "path": "/charges", "method": "POST" } },
    { "function": "db_write", "connector": "orders-db", "config": { "query": "INSERT INTO orders (item_id, quantity, customer_id, charge_id) VALUES ($1, $2, $3, $4)", "params": ["data.item_id", "data.quantity", "data.customer_id", "data.charge_id"] } },
    { "function": "publish_kafka", "connector": "events", "config": { "topic": "order.created" } }
  ]
}
```

**Step 4 — It's live:**
```
POST /api/v1/data/orders  →  validates → checks credit → charges payment → stores order → publishes event
```

## What Changes from Today's Orion

| Aspect | Current (Rules Engine) | Proposed (Services Platform) |
|---|---|---|
| **Channel** | Message topic for rule matching | **Sync** (REST, Simple HTTP) or **Async** (Kafka) — each a microservice boundary |
| **Rules** | JSONLogic condition + task list | **Workflows** — richer orchestration with branching via `dataflow-rs` |
| **Functions** | 10 built-in task types | Extended with `db_read`, `db_write`, `cache_read`, `cache_write`, `mongo_read`, `channel_call` + more via connectors |
| **Connectors** | HTTP and Kafka only | DB, Cache, Storage, MongoDB, and more |
| **Storage backend** | SQLite only | SQLite, PostgreSQL, MySQL — compile-time feature flags |
| **Data flow** | JSON in, JSON out | Any payload — string, blob, JSON, XML — schema optional |
| **Protocols** | POST-only HTTP channels | REST and Simple HTTP (sync) + Kafka (async) |
| **Deployment** | Single instance | Topology-aware: microservice, modular monolith, monolith via channel include/exclude |
| **Channel loading** | All channels always loaded | DB as source of truth, config filters what each instance loads |

## Key Design Principles

1. **Convention over configuration** — sensible defaults, override only when needed
2. **Connectors own complexity** — auth, retries, circuit breaking, pooling are connector concerns, not workflow concerns
3. **Same definition, any topology** — a channel definition doesn't change whether deployed as a microservice or part of a monolith
4. **Observable by default** — every request produces traces, metrics, and logs without extra configuration
5. **Fail safe** — circuit breakers, timeouts, retries, and graceful degradation are built into the platform
6. **Stateless core** — workflows are stateless; state lives in external systems via connectors
7. **API-first** — admin APIs are the primary interface; everything is automatable

## Implementation Status

### Fully Implemented

| Feature | Status | Details |
|---|---|---|
| **Workflow CRUD + lifecycle** | Done | Create, list, get, update, delete, activate, archive, versioning, rollout percentage |
| **Workflow execution** | Done | `dataflow-rs` engine processes workflows with branching, conditions, task pipelines |
| **Workflow dry-run + validate** | Done | Test workflows against sample data; validate schema, tasks, JSONLogic, connector refs |
| **Workflow import/export** | Done | Bulk operations for CI/CD and GitOps |
| **Channel CRUD + lifecycle** | Done | Create, list, get, update, delete, activate, archive, versioning |
| **Channel data model** | Done | Sync/async types, rest/http/kafka protocols, methods, route_pattern, topic, consumer_group, workflow_id binding, per-channel config_json |
| **Connector CRUD** | Done | HTTP, Kafka, DB, Cache, Storage config types with secret masking, auth, retry config |
| **Engine hot-reload** | Done | Loads active channels + workflows, swaps engine atomically, rebuilds channel registry |
| **Channel registry** | Done | In-memory cache populated on reload with parsed `ChannelConfig` per channel |
| **Observability** | Done | Structured logging (JSON/pretty), Prometheus metrics, distributed tracing (W3C/OpenTelemetry), consistent error responses, request IDs |
| **Kafka async consumption** | Done | Config-file-driven topic-to-channel mapping, DLQ support, consumer group |
| **Circuit breakers** | Done | Per-connector/channel, lock-free atomic state machine, admin API to inspect/reset |
| **Trace management** | Done | Sync/async traces with retention, cleanup, list/poll APIs |
| **Health endpoint** | Done | Component-level health checks (DB, engine) with automatic degradation |
| **Per-channel rate limiting** | Done | Middleware reads `ChannelRegistry` per-channel limits with custom key logic (JSONLogic), falls back to platform defaults |
| **Per-channel timeout** | Done | `tokio::time::timeout` wrapper around workflow execution, configurable per channel via `timeout_ms` |
| **Per-channel CORS** | Done | Per-channel origin check in data route handler, rejects unauthorized origins with 403 |
| **Per-channel backpressure** | Done | Semaphore-based concurrency limiter per channel, returns 503 when `max_concurrent` exceeded |
| **Per-channel input validation** | Done | `validation_logic` (JSONLogic) enforced pre-workflow with access to `metadata.headers` (content-type, content-length), `metadata.query`, and `metadata.params` |
| **`channel_call` function handler** | Done | Invokes another channel's workflow in-process via the engine. Supports static/dynamic channel name, custom data, and configurable `response_path`. |
| **REST route matching** | Done | `RouteTable` in `ChannelRegistry` matches `(method, path)` to channels. Path param extraction (e.g., `{id}`), query param extraction, HTTP headers — all injected into metadata for `validation_logic`. Method filtering, priority ordering. Unified `dynamic_handler` with backward-compatible simple HTTP fallback. |
| **DB-driven async channels** | Done | Kafka consumer merges config-file topics with active async channels from DB. Consumer restarted on engine reload when topics change. Config validation allows empty `kafka.topics` when DB channels provide them. |
| **API versioning** | Done | Path-based: channels with versioned route patterns (e.g., `/v1/orders/{id}`, `/v2/orders/{id}`) map to different workflows. Handled natively by the `RouteTable` — no special mechanism needed. |
| **Multi-database backend** | Done | SQLite, PostgreSQL, and MySQL supported as internal storage backends via compile-time feature flags (`db-sqlite`, `db-postgres`, `db-mysql`). Sea-query generates backend-specific SQL. Migrations per backend in `migrations/{sqlite,postgres,mysql}/`. |
| **Schema & migration generator** | Done | `src/storage/schema.rs` defines tables via sea-query. `src/storage/migration_gen.rs` generates backend-specific SQL DDL. Repositories use sea-query for portable query building. |
| **Connection pool caching** | Done | `SqlPoolCache` (SQL), `RedisPoolCache` (Redis), `MongoPoolCache` (MongoDB) provide lazy-loaded, per-connector connection pools with fast-path read lock + slow-path write lock pattern. |
| **Kubernetes health probes** | Done | `GET /healthz` (liveness — always 200) and `GET /readyz` (readiness — checks DB, engine, startup flag). |
| **Admin API authentication** | Done | Bearer token / API-key middleware for all `/api/v1/admin/*` endpoints. Configurable via `[admin_auth]` section. |
| **TLS/HTTPS** | Done | Feature-gated (`--features tls`) via `axum-server` + rustls. Configurable cert/key paths. |
| **Panic recovery** | Done | `CatchPanicLayer` outermost middleware returns JSON error matching `OrionError` format. |

### Data Model Only — Runtime Enforcement Pending

These features have complete data models (`ChannelConfig` structs in `src/channel/mod.rs`) and are stored per-channel in `config_json`, but **no middleware enforces them at request time**.

| Feature | Data Model | Enforcement | What's needed |
|---|---|---|---|
| **Per-channel caching** | `ChannelCacheConfig` | Not implemented | Response cache layer keyed by channel + request hash |
| **Request deduplication** | `DeduplicationConfig` | Not implemented | Idempotency key store (in-memory or DB) with TTL window |
| **Response compression** | `CompressionConfig` | Not implemented | Conditional gzip/brotli compression middleware |
| **Channel include/exclude filtering** | `ChannelLoadingConfig` in config | Not applied | Glob matching in `reload_engine()` before loading channels |

### Handler Code Complete — Not Registered in Engine

These function handlers have complete implementations in `src/engine/functions/` but are **not yet registered** in `build_custom_functions()`. The handler code, pool caching, and connector configs all exist — what remains is wiring them into the engine and adding them to `KNOWN_FUNCTIONS`.

| Feature | Handler File | Feature Flag | What's missing |
|---|---|---|---|
| **`db_read`** | `src/engine/functions/db_read.rs` | `connectors-sql` | Not registered in `build_custom_functions()`. Listed in `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS`. |
| **`db_write`** | `src/engine/functions/db_write.rs` | `connectors-sql` | Not registered in `build_custom_functions()`. Listed in `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS`. |
| **`cache_read`** | `src/engine/functions/cache_read.rs` | `connectors-redis` | Not registered in `build_custom_functions()`. Not in `KNOWN_FUNCTIONS`. |
| **`cache_write`** | `src/engine/functions/cache_write.rs` | `connectors-redis` | Not registered in `build_custom_functions()`. Not in `KNOWN_FUNCTIONS`. |
| **`mongo_read`** | `src/engine/functions/mongo_read.rs` | `connectors-mongodb` | Not registered in `build_custom_functions()`. Not in `KNOWN_FUNCTIONS`. |

## Next Steps — Implementation Roadmap

### Phase 1: Register Connector Function Handlers (Unlocks DB/Cache/MongoDB workflows)

The handler code, pool caches, and connector configs are **all implemented**. What remains is wiring them into the engine so workflows can use them at runtime.

- Register `DbReadHandler` and `DbWriteHandler` in `build_custom_functions()` (feature-gated on `connectors-sql`)
- Register `CacheReadHandler` and `CacheWriteHandler` in `build_custom_functions()` (feature-gated on `connectors-redis`)
- Register `MongoReadHandler` in `build_custom_functions()` (feature-gated on `connectors-mongodb`)
- Add `cache_read`, `cache_write`, `mongo_read` to `KNOWN_FUNCTIONS` and `CONNECTOR_FUNCTIONS`
- Pass `SqlPoolCache`, `RedisPoolCache`, `MongoPoolCache` instances into the handlers (create in `main.rs` and thread through)
- Integration tests for each handler

### Phase 2: Per-Channel Enforcement

These are independent, small-scope items that complete the platform baseline. Each can be done in isolation.

**2a. Response compression** — ~1 hr
- Add conditional gzip/brotli compression in the response path
- Respect `min_bytes` threshold and `enabled` flag from `CompressionConfig`
- Consider using `tower-http::compression` per-channel rather than global

**2b. Request deduplication** — ~2 hrs
- Implement in-memory idempotency key store (dashmap with TTL-based expiry)
- Read `Idempotency-Key` header (configurable name per channel from `DeduplicationConfig`)
- Return cached response within dedup window, skip workflow execution

**2c. Per-channel caching** — ~3 hrs
- Response cache layer keyed by channel + request hash (derived from `cache_key_fields`)
- TTL-based expiry from `ChannelCacheConfig`
- In-memory (dashmap) for single-instance; CacheConnector (Redis) for multi-instance later

**2d. Channel include/exclude filtering** — ~30 min
- Apply `ChannelLoadingConfig` glob patterns in `reload_engine()` before loading channels
- Filter `list_active()` results against include/exclude lists
- Enables microservice topology where each instance loads a subset of channels

## Future Exploration

These capabilities are explicitly out of scope for the initial platform but are anticipated for future phases:

- **GraphQL channels** — query/mutation dispatch to workflows
- **ORM-like DB capabilities** — beyond raw parameterized queries
- **WASM plugin system** — custom functions in any language that compiles to WebAssembly
- **Additional async transports** — AMQP (RabbitMQ), NATS, SQS
- **Built-in auth middleware** — JWT validation, API key enforcement at the channel level
- **Visual workflow builder** — UI built on top of the admin APIs
- **Parallel step execution** — fan-out/fan-in within a single workflow
