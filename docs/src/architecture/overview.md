# Architecture Overview

## Three Primitives

Services in Orion are composed from three building blocks:

| Primitive | Role | Examples |
|-----------|------|----------|
| **Channel** | Service endpoint: sync (REST, HTTP) or async (Kafka) | `POST /orders`, `GET /users/{id}`, Kafka topic `order.placed` |
| **Workflow** | Pipeline of tasks that defines what the service does | Parse → validate → enrich → transform → respond |
| **Connector** | Named connection to an external system with auth and retries | Stripe API, PostgreSQL, Redis, Kafka cluster |

Channels receive traffic. Workflows process it. Connectors reach out to external systems. Everything else (rate limiting, metrics, circuit breakers, versioning) is handled by the platform.

## Deployment Topology

### Before Orion

Every piece of business logic is its own service to build, deploy, and operate, each with its own infrastructure stack:

```orion-diagram
{
  "direction": "TB",
  "groups": [
    { "id": "g_svc1", "label": "Pricing Service (Auto-Scaled)" },
    { "id": "g_svc2", "label": "Fraud Service (Auto-Scaled)" },
    { "id": "g_svc3", "label": "Routing Service (Auto-Scaled)" },
    { "id": "g_svc4", "label": "Notification Service (Auto-Scaled)" },
    { "id": "g_obs", "label": "Observability Stack" },
    { "id": "g_ci", "label": "CI/CD per Service (x4)" },
    { "id": "g_inf", "label": "Shared Platform Infrastructure" }
  ],
  "nodes": [
    { "id": "cdn", "label": "CDN / Edge Cache", "type": "gateway", "shape": "cloud" },
    { "id": "lb", "label": "Load Balancer", "type": "gateway", "shape": "hexagon" },
    { "id": "gw", "label": "API Gateway", "sublabel": "auth · throttle · routing", "type": "gateway", "shape": "hexagon" },

    { "id": "sm1", "label": "Mesh Proxy", "type": "gateway", "group": "g_svc1" },
    { "id": "p1", "label": "Container", "type": "service", "group": "g_svc1" },
    { "id": "p1h", "label": "Health Check", "type": "channel", "group": "g_svc1" },
    { "id": "p1l", "label": "Log Agent", "type": "observability", "group": "g_svc1" },
    { "id": "p1m", "label": "Metrics Agent", "type": "observability", "group": "g_svc1" },

    { "id": "sm2", "label": "Mesh Proxy", "type": "gateway", "group": "g_svc2" },
    { "id": "f1", "label": "Container", "type": "service", "group": "g_svc2" },
    { "id": "f1h", "label": "Health Check", "type": "channel", "group": "g_svc2" },
    { "id": "f1l", "label": "Log Agent", "type": "observability", "group": "g_svc2" },
    { "id": "f1m", "label": "Metrics Agent", "type": "observability", "group": "g_svc2" },

    { "id": "sm3", "label": "Mesh Proxy", "type": "gateway", "group": "g_svc3" },
    { "id": "r1", "label": "Container", "type": "service", "group": "g_svc3" },
    { "id": "r1h", "label": "Health Check", "type": "channel", "group": "g_svc3" },
    { "id": "r1l", "label": "Log Agent", "type": "observability", "group": "g_svc3" },
    { "id": "r1m", "label": "Metrics Agent", "type": "observability", "group": "g_svc3" },

    { "id": "sm4", "label": "Mesh Proxy", "type": "gateway", "group": "g_svc4" },
    { "id": "n1", "label": "Container", "type": "service", "group": "g_svc4" },
    { "id": "n1h", "label": "Health Check", "type": "channel", "group": "g_svc4" },
    { "id": "n1l", "label": "Log Agent", "type": "observability", "group": "g_svc4" },
    { "id": "n1m", "label": "Metrics Agent", "type": "observability", "group": "g_svc4" },

    { "id": "db", "label": "Database", "sublabel": "Primary + Replica", "type": "datastore" },
    { "id": "cache", "label": "Cache Cluster", "type": "datastore" },
    { "id": "mq", "label": "Message Queue", "type": "datastore", "shape": "queue" },
    { "id": "smtp", "label": "Email Service", "type": "datastore", "shape": "cloud" },

    { "id": "log", "label": "Log Aggregator", "type": "observability", "group": "g_obs" },
    { "id": "met", "label": "Metrics Backend", "type": "observability", "group": "g_obs" },
    { "id": "trc", "label": "Trace Collector", "type": "observability", "group": "g_obs" },
    { "id": "dash", "label": "Dashboards & Alerts", "type": "observability", "group": "g_obs" },

    { "id": "reg", "label": "Container Registry", "type": "ci", "group": "g_ci" },
    { "id": "pipe", "label": "Build Pipeline", "type": "ci", "group": "g_ci" },
    { "id": "deploy", "label": "Deploy Controller", "type": "ci", "group": "g_ci" },
    { "id": "canary", "label": "Canary / Rollout", "type": "ci", "group": "g_ci" },

    { "id": "sr", "label": "Service Registry", "type": "infra", "group": "g_inf" },
    { "id": "sec", "label": "Secret Manager", "type": "infra", "group": "g_inf" },
    { "id": "cfg", "label": "Config Server", "type": "infra", "group": "g_inf" },
    { "id": "cert", "label": "Certificate Manager", "type": "infra", "group": "g_inf" }
  ],
  "edges": [
    { "from": "cdn", "to": "lb" }, { "from": "lb", "to": "gw" },
    { "from": "gw", "to": "sm1" }, { "from": "gw", "to": "sm2" }, { "from": "gw", "to": "sm3" }, { "from": "gw", "to": "sm4" },
    { "from": "sm1", "to": "p1" }, { "from": "sm2", "to": "f1" }, { "from": "sm3", "to": "r1" }, { "from": "sm4", "to": "n1" },
    { "from": "p1", "to": "p1h" }, { "from": "p1", "to": "p1l" }, { "from": "p1", "to": "p1m" },
    { "from": "f1", "to": "f1h" }, { "from": "f1", "to": "f1l" }, { "from": "f1", "to": "f1m" },
    { "from": "r1", "to": "r1h" }, { "from": "r1", "to": "r1l" }, { "from": "r1", "to": "r1m" },
    { "from": "n1", "to": "n1h" }, { "from": "n1", "to": "n1l" }, { "from": "n1", "to": "n1m" },
    { "from": "p1", "to": "db" }, { "from": "f1", "to": "cache" }, { "from": "r1", "to": "mq" }, { "from": "n1", "to": "smtp" },
    { "from": "p1l", "to": "log" }, { "from": "f1l", "to": "log" }, { "from": "r1l", "to": "log" }, { "from": "n1l", "to": "log" },
    { "from": "p1m", "to": "met" }, { "from": "f1m", "to": "met" }, { "from": "r1m", "to": "met" }, { "from": "n1m", "to": "met" },
    { "from": "log", "to": "dash" }, { "from": "met", "to": "dash" }, { "from": "trc", "to": "dash" }
  ]
}
```

**4 services x (code + Dockerfile + CI pipeline + health checks + metrics agent + log agent + sidecar proxy + scaling policy + secret config + canary rollout) = dozens of components to build, wire, and keep running.**

### After Orion

One Orion instance replaces all four:

```orion-diagram
{
  "direction": "LR",
  "groups": [ { "id": "g_orion", "label": "Orion Runtime" } ],
  "nodes": [
    { "id": "clients", "label": "Clients", "type": "gateway", "shape": "actor" },
    { "id": "ch1", "label": "/pricing", "type": "channel", "group": "g_orion" },
    { "id": "ch2", "label": "/fraud", "type": "channel", "group": "g_orion" },
    { "id": "ch3", "label": "/routing", "type": "channel", "group": "g_orion" },
    { "id": "ch4", "label": "/notify", "type": "channel", "group": "g_orion" },
    { "id": "wf1", "label": "workflow", "type": "service", "group": "g_orion" },
    { "id": "wf2", "label": "workflow", "type": "service", "group": "g_orion" },
    { "id": "wf3", "label": "workflow", "type": "service", "group": "g_orion" },
    { "id": "wf4", "label": "workflow", "type": "service", "group": "g_orion" },
    { "id": "db", "label": "Database", "type": "datastore" },
    { "id": "rd", "label": "Redis", "type": "datastore" },
    { "id": "kf", "label": "Kafka", "type": "datastore", "shape": "queue" },
    { "id": "smtp", "label": "SMTP", "type": "datastore", "shape": "cloud" }
  ],
  "edges": [
    { "from": "clients", "to": "ch1" }, { "from": "clients", "to": "ch2" }, { "from": "clients", "to": "ch3" }, { "from": "clients", "to": "ch4" },
    { "from": "ch1", "to": "wf1" }, { "from": "ch2", "to": "wf2" }, { "from": "ch3", "to": "wf3" }, { "from": "ch4", "to": "wf4" },
    { "from": "wf1", "to": "db" }, { "from": "wf2", "to": "rd" }, { "from": "wf3", "to": "kf" }, { "from": "wf4", "to": "smtp" }
  ]
}
```

No API gateway needed. Governance is built in. One binary to deploy.

**The best of both worlds:** each channel and workflow is independently versioned, testable, and deployable. The modularity of microservices with the operational simplicity of a monolith.

## Deploy Anywhere

```orion-diagram
{
  "direction": "LR",
  "groups": [
    { "id": "g_standalone", "label": "Standalone" },
    { "id": "g_sidecar", "label": "Sidecar" },
    { "id": "g_docker", "label": "Docker" }
  ],
  "nodes": [
    { "id": "s1", "label": "./orion-server", "sublabel": "That's it.", "type": "infra", "group": "g_standalone" },
    { "id": "app", "label": "Your App", "type": "service", "group": "g_sidecar" },
    { "id": "sc", "label": "Orion", "type": "infra", "group": "g_sidecar" },
    { "id": "d1", "label": "docker run", "sublabel": "orion:latest", "type": "observability", "group": "g_docker" }
  ],
  "edges": [
    { "from": "app", "to": "sc" }
  ]
}
```

Single binary. SQLite by default, no database to provision, no runtime dependencies. Need more scale? Swap to **PostgreSQL** or **MySQL** by changing `storage.url`. No rebuild needed.

Same channel definitions work in any topology: run everything in one instance, split channels across instances with include/exclude filters, or deploy as sidecars.

## Request Processing Flow

```orion-diagram
{
  "direction": "LR",
  "nodes": [
    { "id": "req", "label": "HTTP Request", "type": "service" },
    { "id": "router", "label": "Axum Router", "type": "gateway" },
    { "id": "handler", "label": "Data Route Handler", "type": "gateway" },
    { "id": "resolve", "label": "Route Resolution", "sublabel": "pattern match → channel", "type": "gateway" },
    { "id": "registry", "label": "Channel Registry", "sublabel": "dedup · rate limit · validation", "type": "ci" },
    { "id": "engine", "label": "Engine", "sublabel": "RwLock<Arc<Engine>>", "type": "observability" },
    { "id": "matcher", "label": "Workflow Matcher", "sublabel": "JSONLogic + rollout", "type": "gateway" },
    { "id": "pipeline", "label": "Task Pipeline", "sublabel": "ordered execution", "type": "gateway" },
    { "id": "resp", "label": "JSON Response", "type": "channel" }
  ],
  "edges": [
    { "from": "req", "to": "router" }, { "from": "router", "to": "handler" }, { "from": "handler", "to": "resolve" },
    { "from": "resolve", "to": "registry" }, { "from": "registry", "to": "engine" }, { "from": "engine", "to": "matcher" },
    { "from": "matcher", "to": "pipeline" }, { "from": "pipeline", "to": "resp" }
  ]
}
```

1. **Route Resolution:** REST pattern matching finds the channel, or falls back to name lookup
2. **Channel Registry:** enforces deduplication, rate limits, input validation, backpressure, and checks the response cache
3. **Engine:** the workflow engine sits behind a double-Arc (`Arc<RwLock<Arc<Engine>>>`) allowing zero-downtime swaps
4. **Workflow Matcher:** evaluates JSONLogic conditions and rollout percentages to pick the right workflow
5. **Task Pipeline:** executes functions in order (parse, map, filter, http_call, db_read, etc.)

## Sync and Async

```
Sync     POST /api/v1/data/{channel}         → immediate response
Async    POST /api/v1/data/{channel}/async   → returns trace_id, poll later

REST     GET /api/v1/data/orders/{id}        → matched by route pattern
Kafka    topic: order.placed                 → consumed automatically
```

Sync channels respond immediately. Async channels return a trace ID; poll `GET /api/v1/data/traces/{id}` for results. Kafka channels consume from topics configured in the DB or config file.

**Bridging is a pattern, not a feature.** A sync workflow can `publish_kafka` and return 202. An async channel picks it up from there.

## Service Composition

Most platforms require HTTP calls between services, adding latency, failure modes, and serialization overhead. Orion's `channel_call` invokes another channel's workflow **in-process** with zero network round-trip:

```orion-diagram
{
  "direction": "TB",
  "groups": [ { "id": "wf", "label": "order-processing workflow · POST /orders" } ],
  "nodes": [
    { "id": "p", "label": "parse_json", "sublabel": "extract order data", "type": "service", "group": "wf" },
    { "id": "c1", "label": "channel_call", "type": "service", "group": "wf" },
    { "id": "c2", "label": "channel_call", "type": "service", "group": "wf" },
    { "id": "m", "label": "map", "sublabel": "compute pricing", "type": "service", "group": "wf" },
    { "id": "pub", "label": "publish_json", "sublabel": "return combined result", "type": "service", "group": "wf" },
    { "id": "inv", "label": "inventory-check channel", "type": "channel" },
    { "id": "cust", "label": "customer-lookup channel", "type": "channel" }
  ],
  "edges": [
    { "from": "p", "to": "c1" }, { "from": "c1", "to": "c2" }, { "from": "c2", "to": "m" }, { "from": "m", "to": "pub" },
    { "from": "c1", "to": "inv", "label": "in-process", "style": "dashed" },
    { "from": "c2", "to": "cust", "label": "in-process", "style": "dashed" }
  ]
}
```

Each composed channel has its own workflow, versioning, and governance, but calls between them are function calls, not network hops. Cycle detection prevents infinite recursion.

## Built-in Task Functions

Two sources contribute task functions, all compiled into every binary:

**From dataflow-rs 3.0 (workflow primitives):**

| Function | Description |
|----------|-------------|
| `parse_json` | Parse payload into the data context |
| `parse_xml` | Parse XML payloads into structured JSON |
| `filter` | Allow or halt processing based on JSONLogic conditions |
| `map` | Transform and reshape JSON using JSONLogic expressions |
| `validation` | Enforce required fields and constraints |
| `publish_json` / `publish_xml` | Serialize data to JSON or XML output |
| `log` | Emit structured log entries |

**From Orion (connector- and channel-backed handlers):**

| Function | Description |
|----------|-------------|
| `http_call` | Invoke downstream APIs via an HTTP connector |
| `channel_call` | Invoke another channel's workflow in-process |
| `data_query` / `data_write` | Portable, backend-neutral queries and mutations against SQL, MongoDB, or Elasticsearch connectors (see the [Portable Data Dialect](../reference/data-dialect.md)) |
| `db_read` / `db_write` | Execute raw SQL against a SQL connector, return rows / affected count |
| `cache_read` / `cache_write` | Read/write to an in-memory or Redis cache connector |
| `mongo_read` | Query MongoDB collections with raw find() filters |
| `publish_kafka` | Publish messages via a Kafka connector |

The Orion handlers have machine-readable input schemas surfaced at `GET /api/v1/admin/functions`, and workflow create/update calls validate `function.input` against those schemas with field-pathed `FieldError`s before the workflow can be activated.
